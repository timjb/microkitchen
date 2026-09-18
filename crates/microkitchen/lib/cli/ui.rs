//! Progress output, following microsandbox's CLI (`msb`): an ephemeral
//! braille spinner on stderr while something runs, replaced by a
//! `✓ <past tense> <target> (duration)` line when it succeeds, and per-layer
//! bars while an image is pulled. Spinners only draw when stderr is a
//! terminal; completion lines are printed either way unless `--quiet`.

use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use microsandbox::sandbox::PullProgress;
use owo_colors::OwoColorize;

use super::use_color;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const BRAILLE_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠋"];

const TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Shorter operations get no duration in their completion line.
const SHOW_DURATION_AFTER: Duration = Duration::from_millis(500);

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Ephemeral spinner for an operation that can take a second or longer.
pub struct Spinner {
    pb: Option<ProgressBar>,
    start: Instant,
    target: String,
    quiet: bool,
    _echo_guard: Option<EchoGuard>,
}

/// Header spinner plus one bar per image layer while a sandbox is created.
/// Everything is cleared by [`finish`](Self::finish).
pub struct PullProgressDisplay {
    mp: MultiProgress,
    header: ProgressBar,
    layer_bars: Vec<ProgressBar>,
    reference: String,
    /// What the header says once the image is ready and the VM boots.
    then: String,
    download_style: ProgressStyle,
    materialize_style: ProgressStyle,
    done_style: ProgressStyle,
    _echo_guard: Option<EchoGuard>,
}

/// Disables terminal echo while held, so stray keypresses (Enter) cannot
/// add lines that desync indicatif's cursor tracking and leave ghost lines.
struct EchoGuard {
    original: libc::termios,
    fd: i32,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Spinner {
    /// `label` is the action (`Starting`), `target` its object (the sandbox name).
    pub fn start(label: &str, target: &str) -> Self {
        let (pb, echo_guard) = if std::io::stderr().is_terminal() {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .tick_strings(BRAILLE_TICKS)
                    .template(&format!("   {{spinner}} {label:<12} {{wide_msg}}"))
                    .expect("valid spinner template"),
            );
            pb.set_message(target.to_owned());
            pb.enable_steady_tick(TICK_INTERVAL);
            (Some(pb), EchoGuard::acquire())
        } else {
            (None, None)
        };
        Self {
            pb,
            start: Instant::now(),
            target: target.to_owned(),
            quiet: false,
            _echo_guard: echo_guard,
        }
    }

    /// A spinner that prints nothing, for `--quiet`.
    pub fn quiet() -> Self {
        Self {
            pb: None,
            start: Instant::now(),
            target: String::new(),
            quiet: true,
            _echo_guard: None,
        }
    }

    /// [`start`](Self::start), or [`quiet`](Self::quiet) when `quiet` is set.
    pub fn new(quiet: bool, label: &str, target: &str) -> Self {
        if quiet {
            Self::quiet()
        } else {
            Self::start(label, target)
        }
    }

    /// Replace the spinner with `✓ <past_tense> <target> (duration)`.
    pub fn finish_success(self, past_tense: &str) {
        if let Some(pb) = &self.pb {
            pb.finish_and_clear();
        }
        if !self.quiet {
            success(past_tense, &self.target, self.start.elapsed());
        }
    }

    /// Remove the spinner without a trace. Errors are reported by the
    /// caller, so this is also the failure path.
    pub fn finish_clear(self) {
        if let Some(pb) = &self.pb {
            pb.finish_and_clear();
        }
    }

    /// Spin while `work` runs, then [`finish_success`](Self::finish_success)
    /// or, on error, [`finish_clear`](Self::finish_clear).
    pub async fn run<T, E>(
        self,
        past_tense: &str,
        work: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let result = work.await;
        match result {
            Ok(_) => self.finish_success(past_tense),
            Err(_) => self.finish_clear(),
        }
        result
    }
}

impl PullProgressDisplay {
    /// `reference` is the image; `then` what the header says once the image
    /// is ready and the sandbox boots (`Creating <name>`).
    pub fn new(quiet: bool, reference: &str, then: &str) -> Self {
        let is_tty = !quiet && std::io::stderr().is_terminal();
        let mp = MultiProgress::with_draw_target(if is_tty {
            ProgressDrawTarget::stderr_with_hz(10)
        } else {
            ProgressDrawTarget::hidden()
        });

        let header = mp.add(ProgressBar::new_spinner());
        header.set_style(
            ProgressStyle::default_spinner()
                .tick_strings(BRAILLE_TICKS)
                .template("   {spinner} {msg}")
                .expect("valid spinner template"),
        );
        header.set_message(format!("{:<12} {reference}", "Pulling"));
        header.enable_steady_tick(TICK_INTERVAL);

        let bar = |color: &str| {
            ProgressStyle::default_bar()
                .template(&format!(
                    "     {{prefix}}  {{bar:36.{color}/238}}  {{bytes}}/{{total_bytes}}  {{msg:.{color}}}"
                ))
                .expect("valid bar template")
                .progress_chars("━━╌")
        };

        Self {
            mp,
            header,
            layer_bars: Vec::new(),
            reference: reference.to_owned(),
            then: then.to_owned(),
            download_style: bar("magenta"),
            materialize_style: bar("blue"),
            done_style: ProgressStyle::default_bar()
                .template("     {prefix}  {msg}")
                .expect("valid bar template"),
            _echo_guard: if is_tty { EchoGuard::acquire() } else { None },
        }
    }

    pub fn handle_event(&mut self, event: PullProgress) {
        match event {
            PullProgress::Resolving { .. } => {
                self.header
                    .set_message(format!("{:<12} {}...", "Resolving", self.reference));
            }
            PullProgress::Resolved { layer_count, .. } => {
                self.header.set_message(format!(
                    "{:<12} {} ({layer_count} layer{})",
                    "Pulling",
                    self.reference,
                    plural(layer_count)
                ));
                let width = layer_count.to_string().len();
                for i in 0..layer_count {
                    let pb = self.mp.add(ProgressBar::new(1));
                    pb.set_style(self.download_style.clone());
                    pb.set_prefix(format!("layer {:>width$}/{layer_count}", i + 1));
                    pb.set_message("downloading");
                    self.layer_bars.push(pb);
                }
            }
            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                total_bytes,
                ..
            } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    if let Some(total) = total_bytes {
                        pb.set_length(total);
                    }
                    pb.set_position(downloaded_bytes);
                }
            }
            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_length(downloaded_bytes);
                    pb.set_position(downloaded_bytes);
                }
            }
            PullProgress::LayerDownloadVerifying { layer_index, .. } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_message("verifying");
                }
            }
            PullProgress::LayerMaterializeStarted { layer_index, .. } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_style(self.materialize_style.clone());
                    pb.set_position(0);
                    pb.set_length(1);
                    pb.set_message("materializing");
                }
            }
            PullProgress::LayerMaterializeProgress {
                layer_index,
                bytes_read,
                total_bytes,
            } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_length(total_bytes);
                    pb.set_position(bytes_read);
                }
            }
            PullProgress::LayerMaterializeWriting { layer_index } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_position(pb.length().unwrap_or(0));
                    pb.set_message("writing image");
                }
            }
            PullProgress::LayerMaterializeComplete { layer_index, .. } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_position(pb.length().unwrap_or(0));
                    pb.set_style(self.done_style.clone());
                    pb.set_message(check());
                    pb.tick();
                }
            }
            PullProgress::StitchMergingTrees { layer_count } => {
                self.header.set_message(format!(
                    "{:<12} {} ({layer_count} layer{})",
                    "Merging",
                    self.reference,
                    plural(layer_count)
                ));
            }
            PullProgress::StitchWritingFsmeta => {
                self.header
                    .set_message(format!("{:<12} {}", "Writing fsmeta", self.reference));
            }
            PullProgress::StitchWritingVmdk => {
                self.header
                    .set_message(format!("{:<12} {}", "Writing vmdk", self.reference));
            }
            PullProgress::StitchComplete => {
                self.header
                    .set_message(format!("{:<12} {}", "Stitched", self.reference));
            }
            // The image is ready; what remains is booting the sandbox.
            PullProgress::Complete { .. } => {
                for pb in self.layer_bars.drain(..) {
                    pb.finish_and_clear();
                    self.mp.remove(&pb);
                }
                self.header.set_message(self.then.clone());
            }
        }
    }

    /// Clear all progress output from the terminal.
    pub fn finish(self) {
        let _ = self.mp.clear();
    }
}

impl EchoGuard {
    /// `None` when stdin is not a terminal.
    fn acquire() -> Option<Self> {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return None;
        }
        let fd = stdin.as_raw_fd();
        // SAFETY: termios is plain data, filled in by tcgetattr.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is stdin, a terminal; the pointer is to a live termios.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut modified = original;
        modified.c_lflag &= !libc::ECHO;
        // SAFETY: as above.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &modified) } != 0 {
            return None;
        }
        Some(Self { original, fd })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // Discard what was typed meanwhile so it does not spill into the
        // shell prompt, then restore echo.
        // SAFETY: fd and original come from a successful acquire.
        unsafe {
            libc::tcflush(self.fd, libc::TCIFLUSH);
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// `✓ <verb> <target> (duration)`, the line a finished spinner leaves.
pub fn success(verb: &str, target: &str, elapsed: Duration) {
    let duration = if elapsed > SHOW_DURATION_AFTER {
        let text = format!(" ({})", format_duration(elapsed));
        if use_color() {
            text.dimmed().to_string()
        } else {
            text
        }
    } else {
        String::new()
    };
    eprintln!("   {} {verb:<12} {target}{duration}", check());
}

fn check() -> String {
    if use_color() {
        "✓".green().to_string()
    } else {
        "✓".to_owned()
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// `850ms`, `4.2s`, `1m 5s`.
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if duration.as_millis() >= 1000 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(format_duration(Duration::from_millis(4230)), "4.2s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
    }
}
