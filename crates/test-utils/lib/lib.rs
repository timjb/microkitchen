//! Shared fixtures for microkitchen integration tests.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use tempfile::TempDir;

pub use test_macros::mk_test;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// When set, VM tests use their own microsandbox home instead of the user's.
pub const ISOLATE_ENV: &str = "MK_TEST_ISOLATE_HOME";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A temporary project directory with mise isolated from the user's config.
///
/// Layout: `<root>/project` (the kitchen), `<root>/home` (`MICROKITCHEN_HOME`),
/// `<root>/mise/*` (mise's config, data, state and cache dirs). A sandbox
/// created through [`TestKitchen::up`] is removed on drop.
pub struct TestKitchen {
    root: TempDir,
    bin: PathBuf,
    home: PathBuf,
    has_sandbox: Cell<bool>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl TestKitchen {
    /// `bin` is the microkitchen binary, `env!("CARGO_BIN_EXE_microkitchen")`.
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        let root = Self::temp_root();
        let home = root.path().join("home");
        Self::build(root, bin.into(), home)
    }

    /// A second project sharing `home` (and so the broker) with another kitchen.
    pub fn with_home(bin: impl Into<PathBuf>, home: PathBuf) -> Self {
        Self::build(Self::temp_root(), bin.into(), home)
    }

    fn temp_root() -> TempDir {
        tempfile::Builder::new()
            .prefix("mk-test-")
            .tempdir()
            .expect("creating the test directory")
    }

    /// Approvals are queued for `net decide` instead of denied, since tests
    /// have no desktop.
    fn build(root: TempDir, bin: PathBuf, home: PathBuf) -> Self {
        fs::create_dir_all(root.path().join("project")).expect("creating the project directory");
        fs::create_dir_all(&home).expect("creating the home directory");
        let settings = home.join("config.toml");
        if !settings.exists() {
            fs::write(
                &settings,
                "[approval]\nheadless = \"queue\"\ntimeout_secs = 180\n",
            )
            .expect("writing test settings");
        }
        Self {
            root,
            bin,
            home,
            has_sandbox: Cell::new(false),
        }
    }

    pub fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    pub fn home(&self) -> PathBuf {
        self.home.clone()
    }

    /// Write a file relative to the project, creating parent directories.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.project().join(relative);
        fs::create_dir_all(path.parent().expect("file has a parent"))
            .expect("creating directories");
        fs::write(&path, contents).expect("writing test file");
        path
    }

    pub fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.project().join(relative)).expect("reading test file")
    }

    pub fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.project().join(relative);
        fs::create_dir_all(&path).expect("creating directory");
        path
    }

    /// A microkitchen command run from `cwd` (relative to the project).
    pub fn command<I, S>(&self, cwd: &str, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.bin);
        command
            .args(args)
            .current_dir(self.project().join(cwd))
            .env("MICROKITCHEN_HOME", self.home())
            .env("NO_COLOR", "1")
            .env_remove("MICROKITCHEN_LOG");
        self.isolate_mise(&mut command);
        command
    }

    /// Run a microkitchen command to completion.
    pub fn run(&self, cwd: &str, args: &[&str]) -> Output {
        timed(self.command(cwd, args))
    }

    /// Remove this kitchen's sandbox on drop, for tests that create it
    /// without [`TestKitchen::up`] (e.g. expecting `up` to fail).
    pub fn track_sandbox(&self) {
        self.has_sandbox.set(true);
    }

    /// `microkitchen up --no-shell`, asserting success.
    pub fn up(&self) -> Output {
        self.up_with(|_| {})
    }

    /// Like [`TestKitchen::up`], with extra setup of the command (e.g. host env vars).
    pub fn up_with(&self, configure: impl FnOnce(&mut Command)) -> Output {
        self.has_sandbox.set(true);
        let mut command = self.command("", ["up", "--no-shell"]);
        configure(&mut command);
        let output = timed(command);
        assert!(
            output.status.success(),
            "microkitchen up failed\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// `microkitchen exec -- <args>` (not asserted).
    pub fn exec(&self, args: &[&str]) -> Output {
        let mut all = vec!["exec", "--"];
        all.extend_from_slice(args);
        self.run("", &all)
    }

    /// `microkitchen exec -- <args>` with owned arguments (not asserted).
    pub fn exec_owned(&self, args: Vec<String>) -> Output {
        let mut all = vec!["exec".to_owned(), "--".to_owned()];
        all.extend(args);
        timed(self.command("", all))
    }

    /// Run `exec` on another thread, for flows that wait on an approval.
    pub fn spawn_exec(&self, args: Vec<String>) -> JoinHandle<Output> {
        let mut all = vec!["exec".to_owned(), "--".to_owned()];
        all.extend(args);
        let command = self.command("", all);
        std::thread::spawn(move || timed(command))
    }

    /// Pending approvals (`net pending --json`).
    pub fn pending(&self) -> Vec<Value> {
        let output = self.run("", &["net", "pending", "--json"]);
        assert!(
            output.status.success(),
            "net pending failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("pending approvals as JSON")
    }

    /// Wait up to two minutes for a pending approval matching `predicate`.
    pub fn wait_for_pending(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(found) = self.pending().into_iter().find(|p| predicate(p)) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no matching approval; pending: {:?}",
                self.pending()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Answer a pending approval (`allow`, `deny` or `temp`).
    pub fn decide(&self, pending: &Value, answer: &str) {
        let id = pending["id"].to_string();
        let output = self.run("", &["net", "decide", &id, answer]);
        assert!(
            output.status.success(),
            "net decide failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Point mise at directories inside the fixture and trust the project.
    pub fn isolate_mise(&self, command: &mut Command) {
        let mise = self.root.path().join("mise");
        command
            .env("MISE_TRUSTED_CONFIG_PATHS", self.root.path())
            .env("MISE_CONFIG_DIR", mise.join("config"))
            .env("MISE_GLOBAL_CONFIG_FILE", mise.join("config/config.toml"))
            .env("MISE_DATA_DIR", mise.join("data"))
            .env("MISE_STATE_DIR", mise.join("state"))
            .env("MISE_CACHE_DIR", mise.join("cache"))
            .env_remove("MISE_ENV");
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Drop for TestKitchen {
    fn drop(&mut self) {
        if self.has_sandbox.get() {
            timed(self.command("", ["down", "--purge", "-q"]));
            timed(self.command("", ["broker", "stop", "-q"]));
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Run `command` to completion, reporting how long it took (visible with `--nocapture`).
fn timed(mut command: Command) -> Output {
    let label: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let started = Instant::now();
    let output = command.output().expect("running microkitchen");
    eprintln!(
        "[mk-test] {:>6.1}s  microkitchen {}",
        started.elapsed().as_secs_f64(),
        label.join(" ")
    );
    output
}

/// The candidate names of a pending approval.
pub fn names_of(pending: &Value) -> Vec<String> {
    pending["names"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Stdout of a finished command as a string.
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Under `MK_TEST_ISOLATE_HOME`, point `MSB_HOME` at a test-only directory:
/// `$MK_TEST_MSB_HOME`, else `$HOME/.cache/microkitchen-test-msb`.
///
/// The directory persists across runs so the base image is pulled once, and
/// lives on a real disk (images and flat root disks do not fit a tmpfs). The
/// installed `msb` and libkrunfw are reused through `MSB_PATH` and
/// `MSB_LIBKRUNFW_PATH`, which the isolated home does not contain.
pub fn init_isolated_home() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        if std::env::var_os(ISOLATE_ENV).is_none() {
            return;
        }
        let env = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let original = env("MSB_HOME").or_else(|| env("HOME").map(|h| h.join(".microsandbox")));
        let msb = env("MSB_PATH").or_else(|| find_on_path("msb")).or_else(|| {
            original
                .as_ref()
                .map(|o| o.join("bin/msb"))
                .filter(|p| p.exists())
        });
        let libkrunfw = env("MSB_LIBKRUNFW_PATH").or_else(|| {
            original
                .as_ref()
                .map(|o| o.join("lib/libkrunfw.so"))
                .filter(|p| p.exists())
        });

        let home = env("MK_TEST_MSB_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".cache/microkitchen-test-msb")))
            .expect("set MK_TEST_MSB_HOME or HOME");
        fs::create_dir_all(&home).expect("creating the isolated microsandbox home");

        // SAFETY: runs once, at the start of the first test, before the test
        // spawns threads that read the environment.
        unsafe {
            std::env::set_var("MSB_HOME", &home);
            if let Some(msb) = msb {
                std::env::set_var("MSB_PATH", msb);
            }
            if let Some(libkrunfw) = libkrunfw {
                std::env::set_var("MSB_LIBKRUNFW_PATH", libkrunfw);
            }
        }
    });
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
