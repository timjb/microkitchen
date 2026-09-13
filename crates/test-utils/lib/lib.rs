//! Shared fixtures for microkitchen integration tests.

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

/// When set, VM tests run against temporary `MSB_HOME` and `MICROKITCHEN_HOME`.
pub const ISOLATE_ENV: &str = "MK_TEST_ISOLATE_HOME";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A temporary project directory with mise isolated from the user's config.
///
/// Layout: `<root>/project` (the kitchen), `<root>/home` (`MICROKITCHEN_HOME`),
/// `<root>/mise/*` (mise's config, data, state and cache dirs).
pub struct TestKitchen {
    root: TempDir,
    bin: PathBuf,
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
// Functions
//--------------------------------------------------------------------------------------------------

/// Under `MK_TEST_ISOLATE_HOME`, point `MSB_HOME` and `MICROKITCHEN_HOME` at a
/// temporary directory shared by the test binary, reusing the installed `msb`.
pub fn init_isolated_home() {
    static HOME: OnceLock<Option<TempDir>> = OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var_os(ISOLATE_ENV)?;
        let dir = tempfile::Builder::new()
            .prefix("mk-home-")
            .tempdir()
            .expect("creating the isolated home");
        let msb = find_on_path("msb");
        // SAFETY: runs once, at the start of the first test, before the test
        // spawns threads that read the environment.
        unsafe {
            std::env::set_var("MSB_HOME", dir.path().join("msb"));
            std::env::set_var("MICROKITCHEN_HOME", dir.path().join("microkitchen"));
            if std::env::var_os("MSB_PATH").is_none()
                && let Some(msb) = msb
            {
                std::env::set_var("MSB_PATH", msb);
            }
        }
        Some(dir)
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
