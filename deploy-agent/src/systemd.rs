use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::path::Path;
use zbus::{Connection, proxy, zvariant::OwnedObjectPath};

/// D-Bus proxy for systemd Manager interface
#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    /// Start or restart a unit in a given mode (e.g., "replace")
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    /// Reload systemd manager configuration (daemon-reload equivalent)
    fn reload(&self) -> zbus::Result<()>;

    /// Signal emitted when a job is removed from systemd's queue
    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

pub struct SystemdClient {
    conn: Connection,
}

impl SystemdClient {
    pub async fn connect_system() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("failed to connect to system D-Bus")?;
        Ok(Self { conn })
    }

    /// Syncs unit files from the extracted release into /etc/deploy-agent/units/<project>/
    pub fn sync_unit_files(
        config_root: &Path,
        project: &str,
        extracted_dir: &Path,
        manifest_units: &[deploy_common::manifest::UnitEntry],
    ) -> Result<bool> {
        let mut changed = false;
        let target_dir = config_root.join("units").join(project);
        std::fs::create_dir_all(&target_dir)?;

        for unit in manifest_units {
            let src_path = extracted_dir.join(&unit.path);
            let dest_path = target_dir.join(&unit.name);

            let new_content = std::fs::read(&src_path)
                .with_context(|| format!("reading extracted unit {}", src_path.display()))?;

            let content_changed = if dest_path.exists() {
                let existing = std::fs::read(&dest_path)?;
                existing != new_content
            } else {
                true
            };

            if content_changed {
                std::fs::write(&dest_path, new_content)
                    .with_context(|| format!("writing unit file {}", dest_path.display()))?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Triggers systemd Reload() to execute generators and pick up unit file changes
    pub async fn daemon_reload(&self) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;
        proxy
            .reload()
            .await
            .context("D-Bus Reload() failed (polkit authorization missing for reload)")?;
        tracing::info!("systemd daemon-reload completed via D-Bus");
        Ok(())
    }

    /// Triggers unit restart and awaits the JobRemoved signal for completion confirmation
    pub async fn restart_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;

        // Subscribe to JobRemoved signals before triggering job to avoid missing fast completions
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = proxy
            .restart_unit(unit_name, "replace")
            .await
            .with_context(|| format!("D-Bus call RestartUnit for '{unit_name}' failed"))?;

        tracing::info!("started job {job_path} for unit '{unit_name}', awaiting JobRemoved...");

        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("unit '{unit_name}' restart completed successfully");
                    return Ok(());
                } else {
                    bail!(
                        "job for unit '{unit_name}' failed with result status: '{}'",
                        args.result
                    );
                }
            }
        }

        bail!("JobRemoved signal stream ended prematurely for '{unit_name}'");
    }
}
