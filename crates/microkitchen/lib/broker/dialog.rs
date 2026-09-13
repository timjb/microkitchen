//! Desktop approval dialogs (design §10).
//!
//! Three answers — Deny, Allow, Allow 5 min. Closing a dialog or letting it
//! time out denies this flow only and is never remembered. The queue in
//! [`super::approval`] shows one dialog at a time.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::approval::{Outcome, PromptInfo};
use super::attribution::Origin;
use super::protocol::Transport;
use crate::state::settings::DialogSetting;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

pub const TITLE: &str = "microkitchen: allow network access?";

const ALLOW: &str = "Allow";
const TEMP: &str = "Allow 5 min";
const DENY: &str = "Deny";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Where approvals are shown to a human.
pub trait Surface: Send + Sync {
    /// Show one approval; resolves to the answer, or `Dismissed`.
    fn show<'a>(
        &'a self,
        info: &'a PromptInfo,
        origin: Option<&'a Origin>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;

    /// A one-off notice (e.g. a tripped rate limit). Best effort.
    fn notify(&self, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Zenity,
    Kdialog,
    Osascript,
}

/// Dialogs through a desktop tool.
pub struct Desktop {
    backend: Backend,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Backend {
    pub fn program(self) -> &'static str {
        match self {
            Self::Zenity => "zenity",
            Self::Kdialog => "kdialog",
            Self::Osascript => "osascript",
        }
    }

    /// Arguments showing `text` for at most `timeout_secs`.
    pub fn args(self, text: &str, timeout_secs: u64) -> Vec<String> {
        let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Self::Zenity => {
                let mut args = owned(&[
                    "--question",
                    "--title",
                    TITLE,
                    "--text",
                    text,
                    "--no-markup",
                    "--switch",
                    "--extra-button",
                    DENY,
                    "--extra-button",
                    TEMP,
                    "--extra-button",
                    ALLOW,
                    "--timeout",
                ]);
                args.push(timeout_secs.to_string());
                args
            }
            // kdialog has no timeout; the queue closes the dialog at the deadline.
            // A menu, unlike --yesnocancel, lets closing the window mean nothing.
            Self::Kdialog => owned(&[
                "--title", TITLE, "--menu", text, "allow", ALLOW, "temp", TEMP, "deny", DENY,
            ]),
            Self::Osascript => {
                let script = format!(
                    "display dialog \"{}\" with title \"{}\" buttons {{\"{DENY}\", \"{TEMP}\", \"{ALLOW}\"}} \
                     default button \"{DENY}\" giving up after {timeout_secs}",
                    applescript_escape(text),
                    applescript_escape(TITLE),
                );
                vec!["-e".into(), script]
            }
        }
    }

    /// The answer in a finished dialog's output.
    pub fn parse(self, success: bool, stdout: &str) -> Outcome {
        let stdout = stdout.trim();
        match self {
            Self::Zenity => label_outcome(stdout),
            Self::Kdialog if success => match stdout {
                "allow" => Outcome::Allow,
                "temp" => Outcome::Temp,
                "deny" => Outcome::Deny,
                _ => Outcome::Dismissed,
            },
            Self::Kdialog => Outcome::Dismissed,
            Self::Osascript => {
                if stdout.contains("gave up:true") {
                    return Outcome::Dismissed;
                }
                stdout
                    .split_once("button returned:")
                    .map(|(_, rest)| label_outcome(rest.split(',').next().unwrap_or("").trim()))
                    .unwrap_or(Outcome::Dismissed)
            }
        }
    }
}

impl Desktop {
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The dialog backend to use, if any. `auto` picks osascript on macOS and,
/// with a display, zenity or kdialog elsewhere.
pub fn detect(setting: DialogSetting) -> Option<Backend> {
    let wanted = match setting {
        DialogSetting::None => return None,
        DialogSetting::Zenity => Some(Backend::Zenity),
        DialogSetting::Kdialog => Some(Backend::Kdialog),
        DialogSetting::Osascript => Some(Backend::Osascript),
        DialogSetting::Auto => None,
    };
    if let Some(backend) = wanted {
        if on_path(backend.program()) {
            return Some(backend);
        }
        tracing::warn!(
            program = backend.program(),
            "approval.dialog names a program that is not on PATH; no dialogs"
        );
        return None;
    }
    if cfg!(target_os = "macos") {
        return on_path("osascript").then_some(Backend::Osascript);
    }
    let display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()));
    if !display {
        return None;
    }
    [Backend::Zenity, Backend::Kdialog]
        .into_iter()
        .find(|b| on_path(b.program()))
}

/// The dialog body (design §10). A never-resolved address is called out
/// rather than left as an empty field.
pub fn render(info: &PromptInfo, origin: Option<&Origin>) -> String {
    let transport = match info.transport {
        Transport::Tcp => "TCP",
        Transport::Udp => "UDP",
    };
    let mut text = String::new();
    let _ = writeln!(text, "Sandbox:      {}", info.sandbox);
    let _ = writeln!(
        text,
        "Destination:  {} : {}  ({transport})",
        info.address, info.port
    );
    match info.names.split_first() {
        None => {
            let _ = writeln!(
                text,
                "Resolved as:  ⚠ this sandbox never resolved this address"
            );
        }
        Some((first, rest)) => {
            let _ = writeln!(text, "Resolved as:  {first}");
            if !rest.is_empty() {
                let _ = writeln!(text, "              also seen as: {}", rest.join(", "));
            }
        }
    }
    if let Some(origin) = origin {
        let _ = writeln!(text, "Process:      {origin}");
    }
    text.trim_end().to_owned()
}

fn label_outcome(label: &str) -> Outcome {
    match label {
        ALLOW => Outcome::Allow,
        TEMP => Outcome::Temp,
        DENY => Outcome::Deny,
        _ => Outcome::Dismissed,
    }
}

fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Surface for Desktop {
    fn show<'a>(
        &'a self,
        info: &'a PromptInfo,
        origin: Option<&'a Origin>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let text = render(info, origin);
            let mut command = Command::new(self.backend.program());
            command
                .args(self.backend.args(&text, timeout.as_secs().max(1)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                // Dropping this future (flow gone, answered elsewhere, deadline)
                // closes the dialog.
                .kill_on_drop(true);
            match command.output().await {
                Ok(output) => self.backend.parse(
                    output.status.success(),
                    &String::from_utf8_lossy(&output.stdout),
                ),
                Err(error) => {
                    tracing::error!(%error, program = self.backend.program(), "cannot show the approval dialog");
                    Outcome::Dismissed
                }
            }
        })
    }

    fn notify(&self, message: &str) {
        let (program, args): (&str, Vec<String>) = match self.backend {
            Backend::Osascript => (
                "osascript",
                vec![
                    "-e".into(),
                    format!(
                        "display notification \"{}\" with title \"microkitchen\"",
                        applescript_escape(message)
                    ),
                ],
            ),
            Backend::Zenity | Backend::Kdialog if on_path("notify-send") => (
                "notify-send",
                vec!["microkitchen".into(), message.to_owned()],
            ),
            Backend::Zenity => (
                "zenity",
                vec!["--notification".into(), "--text".into(), message.to_owned()],
            ),
            Backend::Kdialog => (
                "kdialog",
                vec!["--passivepopup".into(), message.to_owned(), "10".into()],
            ),
        };
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
            Err(error) => tracing::warn!(%error, program, "cannot show a notification"),
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::approval::OriginSlot;

    fn info(names: &[&str]) -> PromptInfo {
        PromptInfo {
            sandbox: "mk-api-1a2b3c4d".into(),
            transport: Transport::Tcp,
            address: "140.82.121.3".parse().unwrap(),
            port: 443,
            names: names.iter().map(|s| s.to_string()).collect(),
            origin: OriginSlot::default(),
        }
    }

    #[test]
    fn renders_the_design_layout() {
        let origin = Origin {
            pid: 412,
            name: "node".into(),
        };
        assert_eq!(
            render(
                &info(&["api.example.com", "example.map.cdn.net"]),
                Some(&origin)
            ),
            "Sandbox:      mk-api-1a2b3c4d\n\
             Destination:  140.82.121.3 : 443  (TCP)\n\
             Resolved as:  api.example.com\n\
             \x20             also seen as: example.map.cdn.net\n\
             Process:      pid 412 (node)"
        );
        let unresolved = render(&info(&[]), None);
        assert!(unresolved.contains("never resolved this address"));
        assert!(!unresolved.contains("Process:"));
    }

    #[test]
    fn zenity_answers() {
        let args = Backend::Zenity.args("text", 60);
        assert!(args.windows(2).any(|w| w == ["--timeout", "60"]));
        assert!(args.contains(&"--no-markup".to_string()));
        assert_eq!(Backend::Zenity.parse(false, "Allow\n"), Outcome::Allow);
        assert_eq!(Backend::Zenity.parse(false, "Allow 5 min"), Outcome::Temp);
        assert_eq!(Backend::Zenity.parse(false, "Deny"), Outcome::Deny);
        // Closed (exit 1) or timed out (exit 5): nothing on stdout.
        assert_eq!(Backend::Zenity.parse(false, ""), Outcome::Dismissed);
    }

    #[test]
    fn kdialog_answers() {
        assert_eq!(Backend::Kdialog.parse(true, "temp\n"), Outcome::Temp);
        assert_eq!(Backend::Kdialog.parse(true, "deny"), Outcome::Deny);
        assert_eq!(Backend::Kdialog.parse(false, ""), Outcome::Dismissed);
        assert_eq!(Backend::Kdialog.parse(false, "allow"), Outcome::Dismissed);
    }

    #[test]
    fn osascript_answers_and_escaping() {
        let args = Backend::Osascript.args("say \"hi\" \\ there", 30);
        assert_eq!(args[0], "-e");
        assert!(args[1].contains("say \\\"hi\\\" \\\\ there"), "{}", args[1]);
        assert!(args[1].ends_with("giving up after 30"));
        assert_eq!(
            Backend::Osascript.parse(true, "button returned:Allow, gave up:false"),
            Outcome::Allow
        );
        assert_eq!(
            Backend::Osascript.parse(true, "button returned:Allow 5 min, gave up:false"),
            Outcome::Temp
        );
        assert_eq!(
            Backend::Osascript.parse(true, "button returned:, gave up:true"),
            Outcome::Dismissed
        );
    }

    #[test]
    fn none_means_no_dialogs() {
        assert_eq!(detect(DialogSetting::None), None);
    }
}
