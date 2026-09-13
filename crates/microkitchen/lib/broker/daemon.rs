//! The broker daemon (`microkitchen broker run`): one per microkitchen home,
//! serving the admin socket and every registered sandbox.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use super::protocol::{Request, Response};
use super::registry::Broker;
use crate::state::Home;
use crate::state::settings::Settings;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub fn socket_path(home: &Home) -> PathBuf {
    home.broker_dir().join("admin.sock")
}

/// Run until `Shutdown`, SIGINT or SIGTERM.
pub async fn run(home: Home) -> Result<()> {
    let dir = home.broker_dir();
    home.ensure()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }

    let lock_path = dir.join("broker.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if lock.try_lock().is_err() {
        bail!("a broker is already running for {}", home.root().display());
    }

    let socket = socket_path(&home);
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    }

    let settings = Settings::load(&home)?;
    let broker = Broker::new(home.clone(), &settings)?;
    broker.restore().await;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "broker listening");

    let shutdown = CancellationToken::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let (broker, shutdown) = (broker.clone(), shutdown.clone());
                    tokio::spawn(async move {
                        if let Err(error) = serve_admin(broker, stream, shutdown).await {
                            tracing::debug!(%error, "admin connection ended with an error");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "admin accept failed"),
            },
        }
    }

    let _ = fs::remove_file(&socket);
    drop(lock);
    tracing::info!("broker stopped");
    Ok(())
}

async fn serve_admin(
    broker: Arc<Broker>,
    stream: UnixStream,
    shutdown: CancellationToken,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => dispatch(&broker, request, &shutdown).await,
            Err(error) => Response::failure(format!("invalid request: {error}")),
        };
        let mut out = serde_json::to_vec(&response)?;
        out.push(b'\n');
        writer.write_all(&out).await?;
    }
    Ok(())
}

async fn dispatch(
    broker: &Arc<Broker>,
    request: Request,
    shutdown: &CancellationToken,
) -> Response {
    match request {
        Request::Ping => Response::success(std::process::id()),
        Request::Register {
            name,
            kitchen_file,
            mode,
            resolver_port,
            proxy_port,
        } => result(
            broker
                .register(&name, kitchen_file, mode, resolver_port, proxy_port)
                .await,
        ),
        Request::Retire { name } => Response::success(broker.retire(&name)),
        Request::SetMode { name, mode } => result(broker.set_mode(&name, mode)),
        Request::Grant { name, subject } => result(broker.grant(&name, &subject)),
        Request::Resume { name } => result(broker.resume(&name)),
        Request::List => Response::success(broker.list()),
        Request::Pending => Response::success(broker.pending()),
        Request::Decide { id, answer } => {
            if broker.decide(id, answer) {
                Response::success(true)
            } else {
                Response::failure(format!("no pending approval {id}"))
            }
        }
        Request::Bindings { name } => result(broker.bindings(&name)),
        Request::Shutdown => {
            shutdown.cancel();
            Response::success(true)
        }
    }
}

fn result<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => Response::success(value),
        Err(error) => Response::failure(format!("{error:#}")),
    }
}
