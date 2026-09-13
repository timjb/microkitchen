//! Talking to the broker from the CLI, and starting it on demand.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::daemon::socket_path;
use super::protocol::{
    Answer, BindingInfo, Mode, PendingApproval, Registration, Request, Response, SandboxInfo,
};
use crate::state::Home;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const START_TIMEOUT: Duration = Duration::from_secs(10);

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub struct BrokerClient {
    home: Home,
    socket: PathBuf,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl BrokerClient {
    pub fn new(home: &Home) -> Self {
        Self {
            home: home.clone(),
            socket: socket_path(home),
        }
    }

    /// Connect, starting `microkitchen broker run` in the background if needed.
    pub async fn ensure_running(home: &Home) -> Result<Self> {
        let client = Self::new(home);
        if client.is_running().await {
            return Ok(client);
        }
        client.spawn()?;
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if client.is_running().await {
                return Ok(client);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "the egress broker did not start; see {}",
            client.log_file().display()
        )
    }

    pub async fn is_running(&self) -> bool {
        self.pid().await.is_ok()
    }

    /// The broker daemon's process id.
    pub async fn pid(&self) -> Result<u32> {
        self.request(&Request::Ping).await
    }

    pub fn log_file(&self) -> PathBuf {
        self.home.logs_dir().join("broker.log")
    }

    pub async fn register(
        &self,
        name: &str,
        kitchen_file: &Path,
        mode: Mode,
        resolver_port: Option<u16>,
        proxy_port: Option<u16>,
    ) -> Result<Registration> {
        self.request(&Request::Register {
            name: name.to_owned(),
            kitchen_file: kitchen_file.to_owned(),
            mode,
            resolver_port,
            proxy_port,
        })
        .await
    }

    pub async fn retire(&self, name: &str) -> Result<bool> {
        self.request(&Request::Retire {
            name: name.to_owned(),
        })
        .await
    }

    pub async fn set_mode(&self, name: &str, mode: Mode) -> Result<()> {
        self.request(&Request::SetMode {
            name: name.to_owned(),
            mode,
        })
        .await
    }

    pub async fn grant(&self, name: &str, subject: &str) -> Result<()> {
        self.request(&Request::Grant {
            name: name.to_owned(),
            subject: subject.to_owned(),
        })
        .await
    }

    /// Lift a tripped approval rate limit. True if the sandbox was limited.
    pub async fn resume(&self, name: &str) -> Result<bool> {
        self.request(&Request::Resume {
            name: name.to_owned(),
        })
        .await
    }

    pub async fn list(&self) -> Result<Vec<SandboxInfo>> {
        self.request(&Request::List).await
    }

    pub async fn pending(&self) -> Result<Vec<PendingApproval>> {
        self.request(&Request::Pending).await
    }

    pub async fn decide(&self, id: u64, answer: Answer) -> Result<bool> {
        self.request(&Request::Decide { id, answer }).await
    }

    pub async fn bindings(&self, name: &str) -> Result<Vec<BindingInfo>> {
        self.request(&Request::Bindings {
            name: name.to_owned(),
        })
        .await
    }

    pub async fn shutdown(&self) -> Result<bool> {
        self.request(&Request::Shutdown).await
    }

    async fn request<T: DeserializeOwned>(&self, request: &Request) -> Result<T> {
        let stream = UnixStream::connect(&self.socket).await.context(
            "the egress broker is not running (start it with `microkitchen broker start`)",
        )?;
        let (reader, mut writer) = stream.into_split();
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        writer.write_all(&line).await?;

        let mut response = String::new();
        BufReader::new(reader).read_line(&mut response).await?;
        let response: Response =
            serde_json::from_str(&response).context("invalid response from the broker")?;
        if !response.ok {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "the broker reported an error".into())
            );
        }
        Ok(serde_json::from_value(response.data)?)
    }

    fn spawn(&self) -> Result<()> {
        let logs = self.home.logs_dir();
        fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_file())
            .with_context(|| format!("opening {}", self.log_file().display()))?;
        let exe = std::env::current_exe().context("locating the microkitchen executable")?;

        let mut command = Command::new(exe);
        command
            .arg("--home")
            .arg(self.home.root())
            .args(["broker", "run"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            // Own process group: a Ctrl-C in the terminal must not take egress down.
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.spawn().context("starting the egress broker")?;
        Ok(())
    }
}
