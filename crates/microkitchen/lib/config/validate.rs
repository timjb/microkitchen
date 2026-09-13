//! Checks that need more than the kitchen file: the `[env]` declarations of
//! every loaded mise file, the host environment, and the host filesystem.

use super::schema::ParsedKitchen;
use super::{Diagnostics, SectionLocation, Severity, Source};
use crate::mise::declarations::EnvDeclarations;
use crate::mise::env::ResolvedEnv;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Validate `kitchen` against the environment; `env` is `None` when mise failed.
pub fn check(
    kitchen: &ParsedKitchen,
    source: &Source,
    declarations: &EnvDeclarations,
    env: Option<&ResolvedEnv>,
    diagnostics: &mut Diagnostics,
) {
    let path = kitchen
        .location
        .unwrap_or(SectionLocation::Underscore)
        .table_path();

    for (name, span) in &kitchen.spans.secrets {
        let key = format!("{path}.secrets.{name}");
        if !declarations.contains(name) {
            diagnostics.push(source.diagnostic(
                Severity::Error,
                span.clone(),
                Some(&key),
                format!(
                    "`{name}` is not declared in [env] of any loaded mise config file; \
                     add e.g. `{name} = {{ required = true }}` to [env]"
                ),
            ));
        } else if env.is_some_and(|env| env.value(name).is_none()) {
            diagnostics.push(source.diagnostic(
                Severity::Notice,
                span.clone(),
                Some(&key),
                format!("`{name}` is empty or unset on the host, so this secret is skipped"),
            ));
        }
    }

    for (index, (mount, span)) in kitchen
        .config
        .mounts
        .iter()
        .zip(&kitchen.spans.mounts)
        .enumerate()
    {
        if !mount.host.exists() {
            diagnostics.push(source.diagnostic(
                Severity::Error,
                span.clone(),
                Some(&format!("{path}.mounts[{index}]")),
                format!("host path `{}` does not exist", mount.host.display()),
            ));
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::schema;
    use crate::mise::env::{MiseEnvEntry, merge};

    fn run(text: &str, host: &[(&str, &str)]) -> Diagnostics {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let file = dir.path().join("mise.toml");
        let source = Source::new(&file, text);
        let mut diagnostics = Diagnostics::default();
        let kitchen = schema::parse(&source, dir.path(), &mut diagnostics).unwrap();

        let mut declarations = EnvDeclarations::default();
        declarations.add_document(&file, text).unwrap();
        let output: BTreeMap<String, MiseEnvEntry> = BTreeMap::new();
        let env = merge(output, &declarations, |name| {
            host.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        });
        check(
            &kitchen,
            &source,
            &declarations,
            Some(&env),
            &mut diagnostics,
        );
        diagnostics
    }

    #[test]
    fn secret_must_be_declared() {
        let diagnostics = run(
            "[env]\nA = \"1\"\n\n[_.microkitchen.secrets.TOKEN]\nallow = [\"x.com\"]\n",
            &[],
        );
        let d = diagnostics.iter().next().unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.line, Some(4));
        assert!(d.message.contains("TOKEN = { required = true }"));
    }

    #[test]
    fn empty_secret_is_skipped_with_a_notice() {
        let text =
            "[env]\nT = { default = \"\" }\n[_.microkitchen.secrets.T]\nallow = [\"x.com\"]\n";
        let diagnostics = run(text, &[]);
        assert_eq!(
            diagnostics.iter().next().unwrap().severity,
            Severity::Notice
        );
        assert!(run(text, &[("T", "value")]).is_empty());
    }

    #[test]
    fn mount_sources_must_exist() {
        let diagnostics = run(
            "[_.microkitchen]\nmounts = [\"./src:/a\", \"./nope:/b\"]\n",
            &[],
        );
        let errors: Vec<_> = diagnostics.iter().collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key.as_deref(), Some("_.microkitchen.mounts[1]"));
        assert!(
            errors[0].message.ends_with("nope` does not exist"),
            "{}",
            errors[0].message
        );
    }
}
