//! Sandbox identity, construction and lifecycle.

pub mod bootstrap;
pub mod build;
pub mod lifecycle;
pub mod naming;
pub mod plan;
pub mod remodel;
pub mod staging;

/// Labels put on every sandbox microkitchen creates.
pub mod labels {
    /// Always `"true"`; marks the sandbox as managed by microkitchen.
    pub const MANAGED: &str = "microkitchen.managed";

    /// Absolute path of the kitchen file; used to find a project's sandbox.
    pub const CONFIG: &str = "microkitchen.config";

    /// Hash of the applied normalized configuration.
    pub const CONFIG_HASH: &str = "microkitchen.config-hash";

    /// microkitchen version that created the sandbox.
    pub const VERSION: &str = "microkitchen.version";

    /// `"true"` once `mise bootstrap` succeeded.
    pub const BOOTSTRAPPED: &str = "microkitchen.bootstrapped";
}
