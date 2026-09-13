//! Parsing `[_.microkitchen]` into a normalized [`KitchenConfig`].
//!
//! The walker reports every problem it finds, each at the span of the
//! offending key or value, rather than stopping at the first. Invalid fields
//! keep their defaults so the rest of the table is still checked.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{Document, Item, TableLike};

use super::hostpat::HostPattern;
use super::size::{format_mib, parse_size_mib};
use super::{Diagnostics, SectionLocation, Severity, Source};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

pub const DEFAULT_CPUS: u8 = 2;

/// Ceiling passed to the SDK as `max_cpus`; also the configurable maximum.
pub const MAX_CPUS: u8 = 64;

pub const DEFAULT_MEMORY_MIB: u32 = 4 * 1024;

/// Ceiling passed to the SDK as `max_memory`; also the configurable maximum.
pub const MAX_MEMORY_MIB: u32 = 64 * 1024;

pub const DEFAULT_DISK_MIB: u32 = 10 * 1024;

/// Guest paths microkitchen mounts or writes itself; user mounts may not overlap them.
pub const RESERVED_GUEST_PATHS: &[&str] = &["/root/.cache/mise", "/root/kitchen", "/.msb"];

const SECTION_KEYS: &[&str] = &["cpus", "memory", "disk", "mounts", "network", "secrets"];

const NETWORK_KEYS: &[&str] = &["network", "allow", "deny", "ports"];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// The normalized sandbox configuration. Stored as the "applied" config so
/// `remodel` can diff against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitchenConfig {
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_mib: u32,
    pub mounts: Vec<Mount>,
    pub network: NetworkConfig,
    pub secrets: BTreeMap<String, SecretConfig>,
}

/// A bind mount of a host directory into the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Absolute, lexically normalized host path.
    pub host: PathBuf,
    /// Absolute, lexically normalized guest path.
    pub guest: String,
    pub readonly: bool,
}

/// microsandbox network preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPreset {
    None,
    #[default]
    Public,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub preset: NetworkPreset,
    pub allow: Vec<HostPattern>,
    pub deny: Vec<HostPattern>,
    pub ports: Vec<PortMapping>,
}

/// A published port, bound on host loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortMapping {
    pub host: u16,
    pub guest: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// Hosts a secret may be substituted for. Patterns are handed to microsandbox as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretConfig {
    pub allow: Vec<HostPattern>,
}

/// Result of parsing the kitchen file.
#[derive(Debug, Clone)]
pub struct ParsedKitchen {
    /// `None` when the file has no microkitchen table; defaults apply.
    pub location: Option<SectionLocation>,
    pub config: KitchenConfig,
    pub spans: Spans,
}

/// Spans kept for checks that need the environment or the filesystem.
#[derive(Debug, Clone, Default)]
pub struct Spans {
    /// Parallel to [`KitchenConfig::mounts`].
    pub mounts: Vec<Option<Range<usize>>>,
    /// Span of each secret's key.
    pub secrets: BTreeMap<String, Option<Range<usize>>>,
}

struct Walker<'a> {
    source: &'a Source,
    diagnostics: &'a mut Diagnostics,
    dir: &'a Path,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Parse the microkitchen table of `source`. Relative mount sources are
/// resolved against `kitchen_dir`.
///
/// Returns `None` only when the file is not valid TOML.
pub fn parse(
    source: &Source,
    kitchen_dir: &Path,
    diagnostics: &mut Diagnostics,
) -> Option<ParsedKitchen> {
    let document = match Document::parse(source.text.as_str()) {
        Ok(document) => document,
        Err(error) => {
            diagnostics.push(source.diagnostic(
                Severity::Error,
                error.span(),
                None,
                error.message().to_owned(),
            ));
            return None;
        }
    };

    let root = document.as_table();
    let underscore = root
        .get("_")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get_key_value("microkitchen"));
    let legacy = root.get_key_value("microkitchen");

    let mut walker = Walker {
        source,
        diagnostics,
        dir: kitchen_dir,
    };

    let (location, item) = match (underscore, legacy) {
        (Some((_, item)), Some((legacy_key, _))) => {
            walker.error(
                legacy_key.span(),
                "microkitchen",
                "both [_.microkitchen] and [microkitchen] are present; move everything into [_.microkitchen]",
            );
            (SectionLocation::Underscore, item)
        }
        (Some((_, item)), None) => (SectionLocation::Underscore, item),
        (None, Some((key, item))) => {
            walker.warning(
                key.span(),
                "microkitchen",
                "mise warns about the unknown field [microkitchen]; rename it to [_.microkitchen]",
            );
            (SectionLocation::Legacy, item)
        }
        (None, None) => {
            return Some(ParsedKitchen {
                location: None,
                config: KitchenConfig::default(),
                spans: Spans::default(),
            });
        }
    };

    let mut config = KitchenConfig::default();
    let mut spans = Spans::default();
    let path = location.table_path();
    if let Some(section) = walker.table(item, path) {
        walker.section(section, path, &mut config, &mut spans);
    }

    Some(ParsedKitchen {
        location: Some(location),
        config,
        spans,
    })
}

/// Parse `host:guest[:ro|:rw]`. A relative or `~/` host path is resolved
/// against `base` or `$HOME`.
pub fn parse_mount(spec: &str, base: &Path) -> Result<Mount, String> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, guest, readonly) = match parts.as_slice() {
        [host, guest] => (*host, *guest, false),
        [host, guest, "ro"] => (*host, *guest, true),
        [host, guest, "rw"] => (*host, *guest, false),
        [_, _, option] => {
            return Err(format!(
                "unknown mount option `{option}`; expected `ro` or `rw`"
            ));
        }
        _ => return Err(format!("expected `host:guest[:ro]`, got `{spec}`")),
    };

    if host.is_empty() {
        return Err("the host path is empty".into());
    }
    if !guest.starts_with('/') {
        return Err(format!("the guest path `{guest}` must be absolute"));
    }
    let guest = normalize_lexically(Path::new(guest));
    if guest == Path::new("/") {
        return Err("cannot mount over the guest's root directory".into());
    }

    let host = expand_home(host)?;
    let host = if host.is_absolute() {
        host
    } else {
        base.join(host)
    };

    Ok(Mount {
        host: normalize_lexically(&host),
        guest: guest.to_string_lossy().into_owned(),
        readonly,
    })
}

/// Parse `host:guest[/tcp|/udp]`.
pub fn parse_port(spec: &str) -> Result<PortMapping, String> {
    let (mapping, protocol) = match spec.rsplit_once('/') {
        Some((mapping, "tcp")) => (mapping, Protocol::Tcp),
        Some((mapping, "udp")) => (mapping, Protocol::Udp),
        Some((_, other)) => {
            return Err(format!(
                "unknown protocol `{other}`; expected `tcp` or `udp`"
            ));
        }
        None => (spec, Protocol::Tcp),
    };
    let (host, guest) = mapping
        .split_once(':')
        .ok_or_else(|| format!("expected `host:guest[/udp]`, got `{spec}`"))?;
    Ok(PortMapping {
        host: parse_port_number(host)?,
        guest: parse_port_number(guest)?,
        protocol,
    })
}

/// Resolve `.` and `..` without touching the filesystem.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `name` is a POSIX-style environment variable name.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn parse_port_number(s: &str) -> Result<u16, String> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("`{s}` is not a port number (1-65535)")),
        Ok(port) => Ok(port),
    }
}

fn expand_home(path: &str) -> Result<PathBuf, String> {
    if path == "~" || path.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or("`~` is used but HOME is not set")?;
        Ok(PathBuf::from(home).join(path[1..].trim_start_matches('/')))
    } else {
        Ok(PathBuf::from(path))
    }
}

fn reserved_overlap(guest: &str) -> Option<&'static str> {
    RESERVED_GUEST_PATHS.iter().copied().find(|reserved| {
        guest == *reserved
            || guest.starts_with(&format!("{reserved}/"))
            || reserved.starts_with(&format!("{guest}/"))
    })
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Walker<'_> {
    fn report(
        &mut self,
        severity: Severity,
        span: Option<Range<usize>>,
        key: &str,
        message: impl Into<String>,
    ) {
        let diagnostic = self.source.diagnostic(severity, span, Some(key), message);
        self.diagnostics.push(diagnostic);
    }

    fn error(&mut self, span: Option<Range<usize>>, key: &str, message: impl Into<String>) {
        self.report(Severity::Error, span, key, message);
    }

    fn warning(&mut self, span: Option<Range<usize>>, key: &str, message: impl Into<String>) {
        self.report(Severity::Warning, span, key, message);
    }

    fn table<'i>(&mut self, item: &'i Item, key: &str) -> Option<&'i dyn TableLike> {
        let table = item.as_table_like();
        if table.is_none() {
            self.error(
                item.span(),
                key,
                format!("expected a table, found {}", item.type_name()),
            );
        }
        table
    }

    fn key_span(table: &dyn TableLike, name: &str) -> Option<Range<usize>> {
        table
            .get_key_value(name)
            .and_then(|(key, item)| key.span().or_else(|| item.span()))
    }

    fn reject_unknown(&mut self, table: &dyn TableLike, path: &str, allowed: &[&str]) {
        for (name, _) in table.iter() {
            if !allowed.contains(&name) {
                self.error(
                    Self::key_span(table, name),
                    &format!("{path}.{name}"),
                    format!(
                        "unknown key `{name}`; expected one of: {}",
                        allowed.join(", ")
                    ),
                );
            }
        }
    }

    fn string<'i>(&mut self, item: &'i Item, key: &str) -> Option<&'i str> {
        let value = item.as_str();
        if value.is_none() {
            self.error(
                item.span(),
                key,
                format!("expected a string, found {}", item.type_name()),
            );
        }
        value
    }

    /// Elements of a string array as `(index, value, span)`.
    fn strings<'i>(
        &mut self,
        item: &'i Item,
        key: &str,
    ) -> Vec<(usize, &'i str, Option<Range<usize>>)> {
        let Some(array) = item.as_array() else {
            self.error(
                item.span(),
                key,
                format!("expected an array of strings, found {}", item.type_name()),
            );
            return Vec::new();
        };
        let mut out = Vec::new();
        for (index, value) in array.iter().enumerate() {
            match value.as_str() {
                Some(s) => out.push((index, s, value.span())),
                None => self.error(
                    value.span(),
                    &format!("{key}[{index}]"),
                    format!("expected a string, found {}", value.type_name()),
                ),
            }
        }
        out
    }

    fn patterns(
        &mut self,
        item: Option<&Item>,
        key: &str,
    ) -> Vec<(HostPattern, Option<Range<usize>>)> {
        let Some(item) = item else {
            return Vec::new();
        };
        let mut out: Vec<(HostPattern, Option<Range<usize>>)> = Vec::new();
        for (index, text, span) in self.strings(item, key) {
            let element = format!("{key}[{index}]");
            match text.parse::<HostPattern>() {
                Ok(pattern) if out.iter().any(|(seen, _)| *seen == pattern) => {
                    self.warning(span, &element, format!("duplicate entry `{pattern}`"));
                }
                Ok(pattern) => out.push((pattern, span)),
                Err(error) => {
                    self.error(span, &element, format!("invalid rule {text:?}: {error}"));
                }
            }
        }
        out
    }

    fn size(
        &mut self,
        section: &dyn TableLike,
        name: &str,
        path: &str,
        max: Option<u32>,
    ) -> Option<u32> {
        let item = section.get(name)?;
        let key = format!("{path}.{name}");
        let text = self.string(item, &key)?;
        match parse_size_mib(text) {
            Ok(mib) if max.is_some_and(|max| mib > max) => {
                let max = format_mib(max.unwrap_or_default());
                self.error(item.span(), &key, format!("must be at most {max}"));
                None
            }
            Ok(mib) => Some(mib),
            Err(error) => {
                self.error(item.span(), &key, error.to_string());
                None
            }
        }
    }

    fn section(
        &mut self,
        section: &dyn TableLike,
        path: &str,
        config: &mut KitchenConfig,
        spans: &mut Spans,
    ) {
        self.reject_unknown(section, path, SECTION_KEYS);

        if let Some(item) = section.get("cpus") {
            let key = format!("{path}.cpus");
            match item.as_integer() {
                Some(n) => match u8::try_from(n) {
                    Ok(cpus) if (1..=MAX_CPUS).contains(&cpus) => config.cpus = cpus,
                    _ => self.error(
                        item.span(),
                        &key,
                        format!("must be between 1 and {MAX_CPUS}, got {n}"),
                    ),
                },
                None => self.error(
                    item.span(),
                    &key,
                    format!("expected an integer, found {}", item.type_name()),
                ),
            }
        }
        if let Some(mib) = self.size(section, "memory", path, Some(MAX_MEMORY_MIB)) {
            config.memory_mib = mib;
        }
        if let Some(mib) = self.size(section, "disk", path, None) {
            config.disk_mib = mib;
        }
        if let Some(item) = section.get("mounts") {
            self.mounts(item, &format!("{path}.mounts"), config, spans);
        }
        if let Some(item) = section.get("network") {
            let key = format!("{path}.network");
            if let Some(network) = self.table(item, &key) {
                self.network(network, &key, &mut config.network);
            }
        }
        if let Some(item) = section.get("secrets") {
            let key = format!("{path}.secrets");
            if let Some(secrets) = self.table(item, &key) {
                self.secrets(secrets, &key, config, spans);
            }
        }
    }

    fn mounts(&mut self, item: &Item, key: &str, config: &mut KitchenConfig, spans: &mut Spans) {
        let mut guests = HashSet::new();
        for (index, text, span) in self.strings(item, key) {
            let element = format!("{key}[{index}]");
            let mount = match parse_mount(text, self.dir) {
                Ok(mount) => mount,
                Err(message) => {
                    self.error(span, &element, message);
                    continue;
                }
            };
            if let Some(reserved) = reserved_overlap(&mount.guest) {
                self.error(
                    span,
                    &element,
                    format!(
                        "guest path `{}` overlaps `{reserved}`, which microkitchen manages",
                        mount.guest
                    ),
                );
            } else if !guests.insert(mount.guest.clone()) {
                self.error(
                    span,
                    &element,
                    format!("guest path `{}` is mounted more than once", mount.guest),
                );
            } else {
                config.mounts.push(mount);
                spans.mounts.push(span);
            }
        }
    }

    fn network(&mut self, network: &dyn TableLike, path: &str, config: &mut NetworkConfig) {
        self.reject_unknown(network, path, NETWORK_KEYS);

        if let Some(item) = network.get("network") {
            let key = format!("{path}.network");
            if let Some(text) = self.string(item, &key) {
                match text {
                    "none" => config.preset = NetworkPreset::None,
                    "public" => config.preset = NetworkPreset::Public,
                    "open" => config.preset = NetworkPreset::Open,
                    other => self.error(
                        item.span(),
                        &key,
                        format!("unknown preset `{other}`; expected `none`, `public` or `open`"),
                    ),
                }
            }
        }

        let allow = self.patterns(network.get("allow"), &format!("{path}.allow"));
        let deny = self.patterns(network.get("deny"), &format!("{path}.deny"));
        for (index, (pattern, span)) in deny.iter().enumerate() {
            if allow.iter().any(|(allowed, _)| allowed == pattern) {
                self.error(
                    span.clone(),
                    &format!("{path}.deny[{index}]"),
                    format!("`{pattern}` is in both `allow` and `deny`"),
                );
            }
        }
        config.allow = allow.into_iter().map(|(p, _)| p).collect();
        config.deny = deny.into_iter().map(|(p, _)| p).collect();

        if let Some(item) = network.get("ports") {
            let key = format!("{path}.ports");
            let mut seen = HashSet::new();
            for (index, text, span) in self.strings(item, &key) {
                let element = format!("{key}[{index}]");
                match parse_port(text) {
                    Ok(port) if !seen.insert((port.host, port.protocol)) => self.error(
                        span,
                        &element,
                        format!("host port {} is published more than once", port.host),
                    ),
                    Ok(port) => config.ports.push(port),
                    Err(message) => self.error(span, &element, message),
                }
            }
        }

        if config.preset == NetworkPreset::None {
            if !config.ports.is_empty() {
                let span = Self::key_span(network, "ports");
                self.error(
                    span,
                    &format!("{path}.ports"),
                    "ports need network access, but `network = \"none\"` disables it",
                );
            }
            if !config.allow.is_empty() || !config.deny.is_empty() {
                let span = Self::key_span(network, "network");
                self.warning(
                    span,
                    &format!("{path}.network"),
                    "`allow` and `deny` have no effect with `network = \"none\"`",
                );
            }
        }
    }

    fn secrets(
        &mut self,
        secrets: &dyn TableLike,
        path: &str,
        config: &mut KitchenConfig,
        spans: &mut Spans,
    ) {
        for (name, item) in secrets.iter() {
            let key = format!("{path}.{name}");
            let key_span = Self::key_span(secrets, name);
            if !is_env_name(name) {
                self.error(
                    key_span,
                    &key,
                    format!("`{name}` is not a valid environment variable name"),
                );
                continue;
            }
            let Some(secret) = self.table(item, &key) else {
                continue;
            };

            for (field, _) in secret.iter().filter(|(field, _)| *field != "allow") {
                let message = if field == "deny" {
                    format!(
                        "secrets have no `deny` list: a secret is only substituted for hosts in `allow`; \
                         to block a host entirely, add it to [{}.network].deny",
                        path.trim_end_matches(".secrets")
                    )
                } else {
                    format!("unknown key `{field}`; expected: allow")
                };
                self.error(
                    Self::key_span(secret, field),
                    &format!("{key}.{field}"),
                    message,
                );
            }

            let allow_key = format!("{key}.allow");
            let allow = match secret.get("allow") {
                None => {
                    self.error(
                        key_span.clone(),
                        &key,
                        "`allow` is required and must list at least one host",
                    );
                    Vec::new()
                }
                Some(item) => {
                    if item.as_array().is_some_and(|a| a.is_empty()) {
                        self.error(item.span(), &allow_key, "must list at least one host");
                    }
                    self.patterns(Some(item), &allow_key)
                }
            };
            let mut hosts = Vec::new();
            for (pattern, span) in allow {
                if pattern.is_name() {
                    hosts.push(pattern);
                } else {
                    self.error(
                        span,
                        &allow_key,
                        format!("`{pattern}`: secret hosts must be names (`host` or `*.suffix`)"),
                    );
                }
            }

            spans.secrets.insert(name.to_owned(), key_span);
            config
                .secrets
                .insert(name.to_owned(), SecretConfig { allow: hosts });
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for KitchenConfig {
    fn default() -> Self {
        Self {
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            disk_mib: DEFAULT_DISK_MIB,
            mounts: Vec::new(),
            network: NetworkConfig::default(),
            secrets: BTreeMap::new(),
        }
    }
}

impl fmt::Display for NetworkPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Public => "public",
            Self::Open => "open",
        })
    }
}

impl fmt::Display for PortMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.guest)?;
        if self.protocol == Protocol::Udp {
            f.write_str("/udp")?;
        }
        Ok(())
    }
}

impl fmt::Display for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → {}", self.host.display(), self.guest)?;
        if self.readonly {
            f.write_str(" (ro)")?;
        }
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> (Option<ParsedKitchen>, Diagnostics) {
        let source = Source::new("/proj/mise.toml", text);
        let mut diagnostics = Diagnostics::default();
        let parsed = parse(&source, Path::new("/proj"), &mut diagnostics);
        (parsed, diagnostics)
    }

    fn errors(diagnostics: &Diagnostics) -> Vec<String> {
        diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.located_message())
            .collect()
    }

    #[test]
    fn full_example_parses() {
        let (parsed, diagnostics) = parse_str(
            r#"
[env]
GITHUB_TOKEN = { required = true }

[_.microkitchen]
cpus = 4
memory = "10G"
disk = "20G"
mounts = ["./src:/app", "/data:/data:ro"]

[_.microkitchen.network]
network = "open"
allow = ["example.com", "*.microsandbox.dev", "203.0.113.7"]
deny = ["potentiallymalicious.com"]
ports = ["8000:8000", "9100:9100/udp"]

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]
"#,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let parsed = parsed.unwrap();
        assert_eq!(parsed.location, Some(SectionLocation::Underscore));
        let config = parsed.config;
        assert_eq!(config.cpus, 4);
        assert_eq!(config.memory_mib, 10240);
        assert_eq!(config.disk_mib, 20480);
        assert_eq!(
            config.mounts,
            vec![
                Mount {
                    host: "/proj/src".into(),
                    guest: "/app".into(),
                    readonly: false
                },
                Mount {
                    host: "/data".into(),
                    guest: "/data".into(),
                    readonly: true
                },
            ]
        );
        assert_eq!(config.network.preset, NetworkPreset::Open);
        assert_eq!(config.network.allow.len(), 3);
        assert_eq!(
            config.network.deny,
            vec![HostPattern::Exact("potentiallymalicious.com".into())]
        );
        assert_eq!(config.network.ports[1].to_string(), "9100:9100/udp");
        assert_eq!(config.secrets["GITHUB_TOKEN"].allow.len(), 2);
        assert!(parsed.spans.secrets["GITHUB_TOKEN"].is_some());
    }

    #[test]
    fn missing_section_means_defaults() {
        let (parsed, diagnostics) = parse_str("[env]\nA = \"1\"\n");
        let parsed = parsed.unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(parsed.location, None);
        assert_eq!(parsed.config, KitchenConfig::default());
    }

    #[test]
    fn legacy_section_warns() {
        let (parsed, diagnostics) = parse_str("[microkitchen]\ncpus = 3\n");
        assert_eq!(parsed.unwrap().config.cpus, 3);
        let warning = diagnostics.iter().next().unwrap();
        assert_eq!(warning.severity, Severity::Warning);
        assert!(warning.message.contains("[_.microkitchen]"));
    }

    #[test]
    fn both_sections_is_an_error() {
        let (_, diagnostics) = parse_str("[microkitchen]\ncpus = 3\n[_.microkitchen]\ncpus = 4\n");
        assert_eq!(errors(&diagnostics).len(), 1);
    }

    #[test]
    fn syntax_errors_are_located() {
        let (parsed, diagnostics) = parse_str("[_.microkitchen]\ncpus = \n");
        assert!(parsed.is_none());
        let d = diagnostics.iter().next().unwrap();
        assert_eq!(d.line, Some(2));
    }

    #[test]
    fn collects_all_errors_with_lines() {
        let (_, diagnostics) = parse_str(
            r#"[_.microkitchen]
cpus = 65
memory = "65G"
disk = "10T"
color = "blue"
mounts = ["relative", "./a:/root/kitchen/x", "./b:/app", "./c:/app", 5]

[_.microkitchen.network]
network = "closed"
allow = ["ok.com", "bad host", "both.com"]
deny = ["both.com"]
ports = ["80", "8000:8000", "8000:9000", "1:2/sctp"]

[_.microkitchen.secrets.TOKEN]
allow = ["github.com", "10.0.0.1"]
deny = ["evil.com"]

[_.microkitchen.secrets.EMPTY]
allow = []

[_.microkitchen.secrets."NOT-VALID"]
allow = ["x.com"]
"#,
        );
        let errors = errors(&diagnostics);
        let expect = [
            ":2:8: _.microkitchen.cpus: must be between 1 and 64, got 65",
            ":3:10: _.microkitchen.memory: must be at most 64G",
            ":4:8: _.microkitchen.disk: unknown unit",
            ":5:1: _.microkitchen.color: unknown key `color`",
            "_.microkitchen.mounts[0]: expected `host:guest[:ro]`",
            "_.microkitchen.mounts[1]: guest path `/root/kitchen/x` overlaps `/root/kitchen`",
            "_.microkitchen.mounts[3]: guest path `/app` is mounted more than once",
            "_.microkitchen.mounts[4]: expected a string, found integer",
            "_.microkitchen.network.network: unknown preset `closed`",
            "_.microkitchen.network.allow[1]: invalid rule \"bad host\"",
            "_.microkitchen.network.deny[0]: `both.com` is in both `allow` and `deny`",
            "_.microkitchen.network.ports[0]: expected `host:guest[/udp]`",
            "_.microkitchen.network.ports[2]: host port 8000 is published more than once",
            "_.microkitchen.network.ports[3]: unknown protocol `sctp`",
            "_.microkitchen.secrets.TOKEN.deny: secrets have no `deny` list",
            "_.microkitchen.secrets.TOKEN.allow: `10.0.0.1`: secret hosts must be names",
            "_.microkitchen.secrets.EMPTY.allow: must list at least one host",
            "_.microkitchen.secrets.NOT-VALID: `NOT-VALID` is not a valid environment variable name",
        ];
        for fragment in expect {
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?} in:\n{}",
                errors.join("\n")
            );
        }
        assert_eq!(errors.len(), expect.len(), "{}", errors.join("\n"));
    }

    #[test]
    fn network_none_rejects_ports() {
        let (_, diagnostics) = parse_str(
            "[_.microkitchen.network]\nnetwork = \"none\"\nports = [\"1:1\"]\nallow = [\"a.com\"]\n",
        );
        assert_eq!(errors(&diagnostics).len(), 1);
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Warning));
    }

    #[test]
    fn mounts_resolve_relative_to_the_kitchen_dir() {
        let base = Path::new("/proj/sub");
        assert_eq!(
            parse_mount("../data:/data", base).unwrap().host,
            Path::new("/proj/data")
        );
        assert_eq!(parse_mount("./x/./y:/x/../y/", base).unwrap().guest, "/y");
        assert!(parse_mount("./x:/", base).is_err());
        assert!(parse_mount(":/x", base).is_err());
        assert!(parse_mount("./x:/x:rx", base).is_err());
        assert!(parse_mount("./x:rel", base).is_err());
    }

    #[test]
    fn env_names() {
        assert!(is_env_name("GITHUB_TOKEN"));
        assert!(is_env_name("_x1"));
        assert!(!is_env_name("1X"));
        assert!(!is_env_name("A-B"));
        assert!(!is_env_name(""));
    }
}
