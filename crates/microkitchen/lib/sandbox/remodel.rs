//! What `microkitchen remodel` changes (implementation plan §9), apart from
//! the SDK calls that apply it.
//!
//! Kitchen-level changes are routed three ways: through the SDK's `modify()`
//! (whose dry run decides live, next start or restart), through the broker
//! (network rules, which it re-reads from the kitchen file), or to a new
//! sandbox (what microsandbox fixes at creation).

use std::collections::BTreeMap;

use crate::config::hostpat::HostPattern;
use crate::config::schema::{KitchenConfig, Mount, NetworkPreset, PortMapping, Protocol};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// How a change reaches the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `modify()`; the SDK's dry run says whether it is live.
    Sdk,
    /// The broker re-reads the kitchen file: live.
    Broker,
    /// Only a new sandbox can have it.
    Recreate,
}

/// One changed field of the kitchen configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub field: String,
    pub before: String,
    pub after: String,
    pub route: Route,
}

/// Environment edits for `modify()`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnvPatch {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl EnvPatch {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Changes from the configuration the sandbox runs with to the desired one.
/// Secret *values* are not part of the configuration; see the caller.
pub fn diff(applied: &KitchenConfig, desired: &KitchenConfig) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut push = |field: &str, before: String, after: String, route: Route| {
        if before != after {
            changes.push(Change {
                field: field.to_owned(),
                before,
                after,
                route,
            });
        }
    };

    push(
        "cpus",
        applied.cpus.to_string(),
        desired.cpus.to_string(),
        Route::Sdk,
    );
    push(
        "memory",
        format_mib(applied.memory_mib),
        format_mib(desired.memory_mib),
        Route::Sdk,
    );
    push(
        "disk",
        format_mib(applied.disk_mib),
        format_mib(desired.disk_mib),
        Route::Sdk,
    );
    push(
        "network.allow",
        patterns(&applied.network.allow),
        patterns(&desired.network.allow),
        Route::Broker,
    );
    push(
        "network.deny",
        patterns(&applied.network.deny),
        patterns(&desired.network.deny),
        Route::Broker,
    );
    push(
        "network",
        preset(applied.network.preset),
        preset(desired.network.preset),
        Route::Recreate,
    );
    push(
        "network.ports",
        ports(&applied.network.ports),
        ports(&desired.network.ports),
        Route::Recreate,
    );
    push(
        "mounts",
        mounts(&applied.mounts),
        mounts(&desired.mounts),
        Route::Recreate,
    );
    push(
        "user",
        applied.user.to_string(),
        desired.user.to_string(),
        Route::Recreate,
    );

    let names = applied.secrets.keys().chain(desired.secrets.keys());
    let mut seen = Vec::new();
    for name in names {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        let hosts = |config: &KitchenConfig| match config.secrets.get(name) {
            Some(secret) => format!("allowed for {}", patterns(&secret.allow)),
            None => "(none)".to_owned(),
        };
        push(
            &format!("secret {name}"),
            hosts(applied),
            hosts(desired),
            Route::Sdk,
        );
    }
    changes
}

/// What the sandbox runs with after applying everything except the changes
/// that need a new sandbox.
pub fn applied_without_recreate(applied: &KitchenConfig, desired: &KitchenConfig) -> KitchenConfig {
    let mut result = desired.clone();
    result.mounts = applied.mounts.clone();
    result.network.preset = applied.network.preset;
    result.network.ports = applied.network.ports.clone();
    result.user = applied.user;
    result
}

/// Environment edits turning `current` into `desired`. Only `managed` keys,
/// the ones microkitchen set from the kitchen file, are ever removed: the
/// sandbox's environment also holds the image's own variables.
pub fn env_patch(
    current: &[(String, String)],
    desired: &BTreeMap<String, String>,
    managed: &[String],
) -> EnvPatch {
    let mut patch = EnvPatch::default();
    for (key, value) in desired {
        if !current.iter().any(|(k, v)| k == key && v == value) {
            patch.set.push((key.clone(), value.clone()));
        }
    }
    for (key, _) in current {
        if managed.contains(key) && !desired.contains_key(key) && !patch.remove.contains(key) {
            patch.remove.push(key.clone());
        }
    }
    patch
}

pub fn format_mib(mib: u32) -> String {
    if mib.is_multiple_of(1024) {
        format!("{}G", mib / 1024)
    } else {
        format!("{mib}M")
    }
}

fn patterns(list: &[HostPattern]) -> String {
    if list.is_empty() {
        return "(none)".to_owned();
    }
    list.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn preset(preset: NetworkPreset) -> String {
    format!("{preset:?}").to_lowercase()
}

fn ports(list: &[PortMapping]) -> String {
    if list.is_empty() {
        return "(none)".to_owned();
    }
    list.iter()
        .map(|p| {
            let protocol = match p.protocol {
                Protocol::Tcp => "",
                Protocol::Udp => "/udp",
            };
            format!("{}:{}{protocol}", p.host, p.guest)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn mounts(list: &[Mount]) -> String {
    if list.is_empty() {
        return "(none)".to_owned();
    }
    list.iter()
        .map(|m| {
            let mode = if m.readonly { " (read-only)" } else { "" };
            format!("{} -> {}{mode}", m.host.display(), m.guest)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::SecretConfig;

    fn config() -> KitchenConfig {
        KitchenConfig::default()
    }

    fn fields(changes: &[Change]) -> Vec<(&str, Route)> {
        changes
            .iter()
            .map(|c| (c.field.as_str(), c.route))
            .collect()
    }

    #[test]
    fn identical_configs_have_no_changes() {
        assert!(diff(&config(), &config()).is_empty());
    }

    #[test]
    fn changes_are_routed() {
        let applied = config();
        let mut desired = config();
        desired.cpus = applied.cpus + 1;
        desired.memory_mib = 2048;
        desired.network.allow = vec!["example.com".parse().unwrap()];
        desired.network.ports = vec![PortMapping {
            host: 8080,
            guest: 80,
            protocol: Protocol::Tcp,
        }];
        desired.mounts = vec![Mount {
            host: "/src".into(),
            guest: "/work".into(),
            readonly: true,
        }];
        desired.user.uid = 2000;
        let changes = diff(&applied, &desired);
        assert_eq!(
            fields(&changes),
            vec![
                ("cpus", Route::Sdk),
                ("memory", Route::Sdk),
                ("network.allow", Route::Broker),
                ("network.ports", Route::Recreate),
                ("mounts", Route::Recreate),
                ("user", Route::Recreate),
            ]
        );
        assert_eq!(changes[1].after, "2G");
        assert_eq!(changes[3].after, "8080:80");
        assert_eq!(changes[4].after, "/src -> /work (read-only)");
        assert_eq!(changes[5].after, "2000:1001");
    }

    #[test]
    fn secrets_are_compared_by_name_and_hosts() {
        let mut applied = config();
        applied.secrets.insert(
            "OLD".into(),
            SecretConfig {
                allow: vec!["a.com".parse().unwrap()],
            },
        );
        applied.secrets.insert(
            "KEPT".into(),
            SecretConfig {
                allow: vec!["a.com".parse().unwrap()],
            },
        );
        let mut desired = config();
        desired.secrets.insert(
            "KEPT".into(),
            SecretConfig {
                allow: vec!["*.a.com".parse().unwrap()],
            },
        );
        desired.secrets.insert(
            "NEW".into(),
            SecretConfig {
                allow: vec!["b.com".parse().unwrap()],
            },
        );
        let changes = diff(&applied, &desired);
        assert_eq!(
            fields(&changes),
            vec![
                ("secret KEPT", Route::Sdk),
                ("secret OLD", Route::Sdk),
                ("secret NEW", Route::Sdk),
            ]
        );
        assert_eq!(changes[1].after, "(none)");
        assert_eq!(changes[2].before, "(none)");
    }

    #[test]
    fn skipping_recreation_keeps_what_the_sandbox_has() {
        let applied = config();
        let mut desired = config();
        desired.cpus = 4;
        desired.network.preset = NetworkPreset::None;
        desired.mounts = vec![Mount {
            host: "/src".into(),
            guest: "/work".into(),
            readonly: false,
        }];
        desired.user.uid = 2000;
        let result = applied_without_recreate(&applied, &desired);
        assert_eq!(result.cpus, 4);
        assert_eq!(result.user, applied.user);
        assert_eq!(result.network.preset, applied.network.preset);
        assert!(result.mounts.is_empty());
        assert!(
            diff(&result, &desired)
                .iter()
                .all(|c| c.route == Route::Recreate)
        );
    }

    #[test]
    fn env_patch_sets_and_removes() {
        let current = vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("A".to_string(), "1".to_string()),
            ("GONE".to_string(), "x".to_string()),
            // Set by the image, not by microkitchen.
            ("DOCKER_VERSION".to_string(), "29".to_string()),
        ];
        let desired = BTreeMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("A".to_string(), "2".to_string()),
            ("B".to_string(), "3".to_string()),
        ]);
        let managed = vec!["A".to_string(), "GONE".to_string()];
        let patch = env_patch(&current, &desired, &managed);
        assert_eq!(
            patch.set,
            vec![("A".into(), "2".into()), ("B".into(), "3".into())]
        );
        assert_eq!(patch.remove, vec!["GONE".to_string()]);

        // Nothing is removed that microkitchen did not set.
        let unchanged = BTreeMap::from([current[0].clone(), current[1].clone()]);
        assert!(env_patch(&current, &unchanged, &[]).is_empty());
        assert!(env_patch(&current[..2], &unchanged, &managed).is_empty());
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_mib(1024), "1G");
        assert_eq!(format_mib(1536), "1536M");
    }
}
