//! Host-side environment resolution.
//!
//! Two mise behaviours shape the merge:
//!
//! - A declared variable that exists on the host and that mise leaves
//!   unchanged (`{ required = true }`, or `{ default }` with a host value) is
//!   omitted from `mise env` output, so it is read from the host directly.
//! - A variable resolving to the empty string counts as absent. That is how
//!   optional variables are expressed (`{ default = "" }`), since mise rejects
//!   `{ required = false }`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::declarations::EnvDeclarations;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One entry of `mise env --json-extended`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MiseEnvEntry {
    pub value: String,
    /// The config or env file the value came from; absent for mise's own
    /// additions such as `PATH`.
    #[serde(default)]
    pub source: Option<PathBuf>,
}

/// Variables to hand to the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolvedEnv {
    /// Non-empty values, as env vars or secrets.
    #[serde(skip)]
    pub values: BTreeMap<String, String>,

    /// Variables that resolved to the empty string and are not forwarded.
    pub empty: BTreeSet<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl ResolvedEnv {
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Merge mise's output with the declarations and the host environment.
pub fn merge(
    output: BTreeMap<String, MiseEnvEntry>,
    declarations: &EnvDeclarations,
    host: impl Fn(&str) -> Option<String>,
) -> ResolvedEnv {
    let mut all: BTreeMap<String, String> = output
        .into_iter()
        .filter(|(name, entry)| entry.source.is_some() && name != "PATH")
        .map(|(name, entry)| (name, entry.value))
        .collect();

    for name in declarations.vars.keys() {
        if !all.contains_key(name)
            && let Some(value) = host(name)
        {
            all.insert(name.clone(), value);
        }
    }

    let mut resolved = ResolvedEnv::default();
    for (name, value) in all {
        if value.is_empty() {
            resolved.empty.insert(name);
        } else {
            resolved.values.insert(name, value);
        }
    }
    resolved
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn merges_output_declarations_and_host() {
        let output: BTreeMap<String, MiseEnvEntry> = serde_json::from_str(
            r#"{
              "FROMFILE": { "source": "/p/.env", "value": "secret" },
              "OPT": { "source": "/p/mise.toml", "value": "" },
              "PATH": { "value": "/usr/bin" },
              "PLAIN": { "source": "/p/mise.toml", "value": "hello" }
            }"#,
        )
        .unwrap();
        let mut declarations = EnvDeclarations::default();
        declarations
            .add_document(
                Path::new("/p/mise.toml"),
                "[env]\nPLAIN = \"hello\"\nOPT = { default = \"\" }\nREQ = { required = true }\nGONE = { default = \"x\" }\nEMPTYHOST = { required = true }\n",
            )
            .unwrap();
        let host = |name: &str| match name {
            "REQ" => Some("from-host".to_owned()),
            "EMPTYHOST" => Some(String::new()),
            "UNDECLARED" => Some("never".to_owned()),
            _ => None,
        };

        let env = merge(output, &declarations, host);
        let values: Vec<_> = env
            .values
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            values,
            [
                ("FROMFILE", "secret"),
                ("PLAIN", "hello"),
                ("REQ", "from-host")
            ]
        );
        assert_eq!(
            env.empty.iter().map(String::as_str).collect::<Vec<_>>(),
            ["EMPTYHOST", "OPT"]
        );
    }

    #[test]
    fn mise_output_wins_over_host() {
        let output = BTreeMap::from([(
            "A".to_owned(),
            MiseEnvEntry {
                value: "mise".into(),
                source: Some("/p/mise.toml".into()),
            },
        )]);
        let mut declarations = EnvDeclarations::default();
        declarations
            .add_document(Path::new("/p/mise.toml"), "[env]\nA = \"mise\"\n")
            .unwrap();
        let env = merge(output, &declarations, |_| Some("host".into()));
        assert_eq!(env.value("A"), Some("mise"));
    }
}
