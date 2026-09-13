//! Finding, creating, starting and removing microkitchen sandboxes.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use microsandbox::Sandbox;
use microsandbox::sandbox::{SandboxHandle, SandboxStatus};

use super::build::{self, GUEST_KITCHEN_DIR, GUEST_MISE_GLOBAL_CONFIG};
use super::labels;
use super::plan::SandboxPlan;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const LIST_PAGE_SIZE: u32 = 100;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The sandbox created for `kitchen_file`, found by its labels.
pub async fn find(kitchen_file: &Path) -> Result<Option<SandboxHandle>> {
    let path = kitchen_file.to_string_lossy().into_owned();
    let page = Sandbox::list_with(|list| {
        list.label(labels::MANAGED, "true")
            .label(labels::CONFIG, path)
            .limit(LIST_PAGE_SIZE)
    })
    .await
    .context("listing sandboxes")?;
    Ok(page.sandboxes.into_iter().next())
}

/// Every sandbox microkitchen created.
pub async fn list_managed() -> Result<Vec<SandboxHandle>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let next = cursor.take();
        let page = Sandbox::list_with(|list| {
            let list = list.label(labels::MANAGED, "true").limit(LIST_PAGE_SIZE);
            match next {
                Some(cursor) => list.cursor(cursor),
                None => list,
            }
        })
        .await
        .context("listing sandboxes")?;
        all.extend(page.sandboxes);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(all),
        }
    }
}

/// A label of a sandbox, if set.
pub fn label(handle: &SandboxHandle, key: &str) -> Option<String> {
    handle.config().ok()?.spec.labels.get(key).cloned()
}

/// Whether the VM is up (or coming up).
pub fn is_active(status: SandboxStatus) -> bool {
    matches!(
        status,
        SandboxStatus::Starting
            | SandboxStatus::Running
            | SandboxStatus::Draining
            | SandboxStatus::Paused
    )
}

/// Lowercase status name for display.
pub fn status_name(status: SandboxStatus) -> String {
    format!("{status:?}").to_lowercase()
}

/// Create the sandbox detached from this process and write the guest config.
pub async fn create(plan: &SandboxPlan) -> Result<Sandbox> {
    let sandbox = build::builder(plan)
        .create_detached()
        .await
        .with_context(|| format!("creating sandbox {}", plan.name))?;
    write_guest_config(&sandbox, &plan.guest_config).await?;
    Ok(sandbox)
}

/// Connect to a running sandbox or start a stopped one.
pub async fn start(handle: &SandboxHandle) -> Result<Sandbox> {
    let result = if is_active(handle.status_snapshot()) {
        handle.connect().await
    } else {
        handle.start_detached().await
    };
    result.with_context(|| format!("starting sandbox {}", handle.name()))
}

/// Stop the sandbox if it is running.
pub async fn stop(handle: &SandboxHandle) -> Result<()> {
    if is_active(handle.status_snapshot()) {
        handle
            .stop()
            .await
            .with_context(|| format!("stopping sandbox {}", handle.name()))?;
    }
    Ok(())
}

/// Stop (or kill, if stopping fails) and remove the sandbox.
pub async fn remove(handle: &SandboxHandle) -> Result<()> {
    if stop(handle).await.is_err() {
        let _ = handle.kill().await;
    }
    handle
        .remove()
        .await
        .with_context(|| format!("removing sandbox {}", handle.name()))
}

/// Write `/root/kitchen/mise.toml` in the guest and make it mise's global config.
pub async fn write_guest_config(sandbox: &Sandbox, contents: &str) -> Result<()> {
    let script = format!(
        "mkdir -p {GUEST_KITCHEN_DIR} \"$(dirname {GUEST_MISE_GLOBAL_CONFIG})\" \
         && cat > {GUEST_KITCHEN_DIR}/mise.toml \
         && ln -sfn {GUEST_KITCHEN_DIR}/mise.toml {GUEST_MISE_GLOBAL_CONFIG}"
    );
    let output = sandbox
        .exec_with("sh", |e| {
            e.args(["-c", script.as_str()])
                .stdin_bytes(contents.as_bytes().to_vec())
        })
        .await
        .context("writing the guest mise.toml")?;
    if !output.status().success {
        bail!(
            "writing the guest mise.toml failed: {}",
            output.stderr().unwrap_or_default().trim()
        );
    }
    Ok(())
}

/// Wait until dockerd answers inside the guest; on timeout, show its log.
pub async fn wait_for_docker(sandbox: &Sandbox, timeout: Duration) -> Result<()> {
    let seconds = timeout.as_secs().max(1);
    let wait =
        format!("timeout {seconds} sh -c 'until docker info >/dev/null 2>&1; do sleep 1; done'");
    let output = sandbox
        .shell(wait)
        .await
        .context("waiting for Docker in the sandbox")?;
    if output.status().success {
        return Ok(());
    }
    let log = sandbox
        .shell(
            "{ journalctl -u docker -n 40 --no-pager || tail -n 40 /var/log/dockerd.err.log; } 2>&1",
        )
        .await
        .ok()
        .and_then(|o| o.stdout().ok())
        .unwrap_or_default();
    bail!("Docker did not become ready within {seconds}s; dockerd log:\n{log}")
}
