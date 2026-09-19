//! Mapping a [`SandboxPlan`] onto the microsandbox SDK builder.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use microsandbox::sandbox::{NetworkPolicy, NetworkProfile, SandboxBuilder, SecretBuilder};
use microsandbox::{Sandbox, SecretSource};
use microsandbox_network::policy::{
    Action, Destination, Direction, PortRange, Protocol as NetworkProtocol, Rule,
};

use super::plan::SandboxPlan;
use crate::broker::attribution;
use crate::config::hostpat::HostPattern;
use crate::config::schema::{MAX_CPUS, MAX_MEMORY_MIB, NetworkPreset, Protocol, SecretConfig};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Base image (product spec).
pub const IMAGE: &str = "cruizba/ubuntu-dind:noble-latest";

/// Guest init. `auto` resolves the image's init, which starts dockerd on every
/// boot. A detached sandbox's startup command runs only when it is created, so
/// Docker must not depend on one.
pub const GUEST_INIT: &str = "auto";

/// Named volume shared by every kitchen for mise's cache.
pub const MISE_CACHE_VOLUME: &str = "microkitchen-mise-cache";

/// mise's cache in the guest (`MISE_CACHE_DIR`), on the shared volume.
pub const MISE_CACHE_GUEST_PATH: &str = "/var/cache/mise";

/// Directory holding the guest copy of the kitchen file.
pub const GUEST_KITCHEN_DIR: &str = "/opt/kitchen";

/// mise's system config in the guest (`MISE_SYSTEM_CONFIG_FILE`): the kitchen
/// file. Not the global config, which stays chef's own for `mise use -g`.
pub const GUEST_MISE_SYSTEM_CONFIG: &str = "/opt/kitchen/mise.toml";

/// mise's installs and shims (`MISE_DATA_DIR`), owned by chef. Outside any
/// home so root and chef see the same tools whatever chef's home is.
pub const GUEST_MISE_DATA_DIR: &str = "/opt/mise";

/// Where the bootstrap installs mise itself.
pub const GUEST_MISE_BIN: &str = "/usr/local/bin/mise";

/// Guest `PATH`: mise's shims ahead of microsandbox's default, so
/// bootstrapped tools work in `exec` and shells without activation.
pub const GUEST_PATH: &str = "/opt/mise/shims:/.msb/scripts:\
/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Guest `TMPDIR`: microsandbox mounts `/tmp` as a tmpfs capped at 512 MiB,
/// too small for some tool installs; `/var/tmp` is on the root disk.
pub const GUEST_TMPDIR: &str = "/var/tmp";

/// Variables microkitchen sets in every guest; the kitchen file's `env` may
/// override them.
pub const GUEST_ENV: &[(&str, &str)] = &[
    ("PATH", GUEST_PATH),
    ("TMPDIR", GUEST_TMPDIR),
    ("MISE_DATA_DIR", GUEST_MISE_DATA_DIR),
    ("MISE_CACHE_DIR", MISE_CACHE_GUEST_PATH),
    ("MISE_SYSTEM_CONFIG_FILE", GUEST_MISE_SYSTEM_CONFIG),
];

/// The user microkitchen's own guest commands run as; everything else runs as
/// the sandbox's default user, chef.
pub const ROOT: &str = "root";

/// Published ports listen on host loopback only.
const PORT_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The SDK builder for `plan`.
///
/// The root disk is flat ext4 so dockerd's overlay storage is not nested on
/// the sandbox's own overlay. The guest `mise.toml` is written after boot
/// (see [`super::lifecycle::write_guest_config`]), not patched in.
pub fn builder(plan: &SandboxPlan) -> SandboxBuilder {
    let config = &plan.config;
    let mut builder = Sandbox::builder(&plan.name)
        .image(IMAGE)
        .root_disk_with(|disk| disk.flat().size(config.disk_mib))
        .cpus(config.cpus)
        .max_cpus(MAX_CPUS)
        .memory(config.memory_mib)
        .max_memory(MAX_MEMORY_MIB)
        .hostname(&plan.name)
        .shell("/bin/bash")
        // chef, by number: it only exists once bootstrap has run.
        .user(config.user.to_string())
        .init(GUEST_INIT)
        // The broker's process attribution helper (design §8).
        .script(attribution::SCRIPT_NAME, attribution::SCRIPT)
        .volume(MISE_CACHE_GUEST_PATH, |mount| {
            mount.named_with(MISE_CACHE_VOLUME, |volume| volume.ensure_exists())
        });

    // The SDK appends rather than replaces, so skip what `plan.env` overrides.
    for (name, value) in GUEST_ENV {
        if !plan.env.contains_key(*name) {
            builder = builder.env(*name, *value);
        }
    }

    for (key, value) in plan.labels() {
        builder = builder.label(key, value);
    }

    for mount in &config.mounts {
        let host = mount.host.clone();
        let readonly = mount.readonly;
        builder = builder.volume(mount.guest.clone(), move |m| {
            let m = m.bind(host);
            if readonly { m.readonly() } else { m }
        });
    }

    builder = match config.network.preset {
        NetworkPreset::None => builder.disable_network(),
        NetworkPreset::Public => {
            builder.network(|n| n.policy(NetworkPolicy::from_profiles([NetworkProfile::Public])))
        }
        NetworkPreset::Open => builder.network(|n| n.policy(NetworkPolicy::allow_all())),
    };

    // The broker is the sandbox's resolver and outbound proxy (design §4);
    // microsandbox keeps the structural denies (design §11).
    if config.network.preset != NetworkPreset::None
        && let Some(egress) = &plan.egress
    {
        let resolver = SocketAddr::from((Ipv4Addr::LOCALHOST, egress.resolver_port));
        builder = builder
            .network(|n| n.dns(|d| d.nameservers([resolver])))
            .proxy(|p| {
                p.socks5(format!("127.0.0.1:{}", egress.proxy_port))
                    .credentials(
                        plan.name.as_str(),
                        SecretSource::env(egress.secret_env.as_str()),
                    )
            })
            .prepend_network_policy_rules(structural_rules());
    }

    for port in &config.network.ports {
        builder = match port.protocol {
            Protocol::Tcp => builder.port_bind(PORT_BIND, port.host, port.guest),
            Protocol::Udp => builder.port_udp_bind(PORT_BIND, port.host, port.guest),
        };
    }

    for (name, value) in &plan.env {
        builder = builder.env(name, value);
    }
    for (name, (value, secret)) in &plan.secrets {
        builder = builder.secret(|s| secret_entry(s, name, value, secret));
    }

    builder
}

/// Evaluated by microsandbox before anything reaches the broker: DNS goes only
/// to the sandbox's own forwarder (and so through the observer), and DNS over
/// TLS is refused (design §11.1).
fn structural_rules() -> Vec<Rule> {
    let deny = |protocols: Vec<NetworkProtocol>, port: u16| Rule {
        direction: Direction::Egress,
        destination: Destination::Any,
        protocols,
        ports: vec![PortRange::single(port)],
        action: Action::Deny,
    };
    vec![
        Rule::allow_dns(),
        deny(vec![NetworkProtocol::Udp, NetworkProtocol::Tcp], 53),
        deny(vec![NetworkProtocol::Tcp], 853),
    ]
}

/// A secret substituted only for its allowed hosts; elsewhere the placeholder
/// passes through unchanged (product spec: `passthrough_all_hosts`).
fn secret_entry(
    builder: SecretBuilder,
    name: &str,
    value: &str,
    secret: &SecretConfig,
) -> SecretBuilder {
    let mut builder = builder.env(name).value(value);
    for pattern in &secret.allow {
        builder = match pattern {
            HostPattern::Exact(host) => builder.allow_host(host),
            HostPattern::Suffix(_) => builder.allow_host_pattern(pattern.to_string()),
            HostPattern::Address(_) | HostPattern::Network(_) => builder,
        };
    }
    builder.on_violation(|v| v.passthrough_all_hosts(true))
}
