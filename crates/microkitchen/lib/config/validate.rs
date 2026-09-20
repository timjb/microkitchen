//! Checks that need more than the kitchen file: the `[env]` declarations of
//! every loaded mise file, the host environment, and the host filesystem.

use std::fs;

use super::schema::ParsedKitchen;
use super::staging::StagePlan;
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

/// Check that every host file a mise entry names can actually be staged.
///
/// Only what staging forces: a source we cannot produce a copy of is an error.
/// What an entry then *does* in the guest is mise's business, so nothing here
/// judges `mode = "track"`, `owner` or templating.
pub fn check_staging(staging: &StagePlan, source: &Source, diagnostics: &mut Diagnostics) {
    for staged in &staging.sources {
        let host = &staged.host;
        let message = match fs::metadata(host) {
            Ok(_) => match fs::read_dir(host).map(|_| ()).or_else(|error| {
                // Not a directory: readable if it opens.
                match error.kind() {
                    std::io::ErrorKind::NotADirectory => fs::File::open(host).map(|_| ()),
                    _ => Err(error),
                }
            }) {
                Ok(()) => continue,
                Err(error) => format!("`{}` cannot be read: {error}", host.display()),
            },
            Err(_) if fs::symlink_metadata(host).is_ok() => {
                format!("`{}` is a symbolic link with no target", host.display())
            }
            Err(_) => format!("host path `{}` does not exist", host.display()),
        };
        diagnostics.push(source.diagnostic(
            Severity::Error,
            staged.span.clone(),
            Some(&staged.key),
            message,
        ));
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

    /// Diagnostics of `check_staging` for a kitchen file in `dir`.
    fn stage(dir: &std::path::Path, text: &str) -> Diagnostics {
        let file = dir.join("mise.toml");
        let source = Source::new(&file, text);
        let mut diagnostics = Diagnostics::default();
        let kitchen = crate::config::schema::parse(&source, dir, &mut diagnostics).unwrap();
        let plan = crate::config::staging::plan(
            &source,
            dir,
            dir,
            kitchen.config.dotfiles.as_deref(),
            &mut diagnostics,
        );
        check_staging(&plan, &source, &mut diagnostics);
        diagnostics
    }

    #[test]
    fn staged_sources_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("present"), "x\n").unwrap();
        let diagnostics = stage(
            dir.path(),
            "[dotfiles]\n\"~/.a\" = \"present\"\n\"~/.b\" = \"missing\"\n",
        );
        let errors: Vec<_> = diagnostics.iter().collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].severity, Severity::Error);
        assert_eq!(errors[0].line, Some(3), "reported at its own line");
        assert_eq!(errors[0].key.as_deref(), Some("dotfiles.\"~/.b\""));
        assert!(errors[0].message.ends_with("does not exist"), "{errors:?}");
    }

    #[test]
    fn a_dangling_symlink_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("nowhere", dir.path().join("link")).unwrap();
        let diagnostics = stage(dir.path(), "[dotfiles]\n\"~/.a\" = \"link\"\n");
        let errors: Vec<_> = diagnostics.iter().collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("symbolic link with no target"),
            "{errors:?}"
        );
    }

    #[test]
    fn the_dotfiles_key_must_point_somewhere() {
        let dir = tempfile::tempdir().unwrap();
        let diagnostics = stage(dir.path(), "[_.microkitchen]\ndotfiles = \"./dots\"\n");
        let errors: Vec<_> = diagnostics.iter().collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.ends_with("does not exist"), "{errors:?}");

        fs::create_dir(dir.path().join("dots")).unwrap();
        assert!(
            stage(dir.path(), "[_.microkitchen]\ndotfiles = \"./dots\"\n").is_empty(),
            "an existing directory is fine"
        );
    }

    /// `mode = "track"`, `owner` and templating are mise's business: staging
    /// only reports what stops it producing a copy.
    #[test]
    fn entries_staging_nothing_are_not_judged() {
        let dir = tempfile::tempdir().unwrap();
        let diagnostics = stage(
            dir.path(),
            "[dotfiles]\n\"~/.zshrc\" = { mode = \"track\", autosave = false }\n\
             \"~/.x\" = { content = \"a\\n\" }\n\
             [bootstrap.files.\"/etc/x\"]\ncontent = \"y\\n\"\nowner = \"root\"\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
