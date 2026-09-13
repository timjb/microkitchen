//! Shared fixtures for microkitchen integration tests.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

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
    has_sandbox: Cell<bool>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl TestKitchen {
    /// `bin` is the microkitchen binary, `env!("CARGO_BIN_EXE_microkitchen")`.
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("mk-test-")
            .tempdir()
            .expect("creating the test directory");
        fs::create_dir_all(root.path().join("project")).expect("creating the project directory");
        Self {
            root,
            bin: bin.into(),
            has_sandbox: Cell::new(false),
        }
    }

    pub fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
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
        self.command(cwd, args)
            .output()
            .expect("running microkitchen")
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
        let output = command.output().expect("running microkitchen up");
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
            let _ = self.command("", ["down", "--purge", "-q"]).output();
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

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
