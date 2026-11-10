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

    /// Stop a unit in a given mode (e.g., "replace")
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    /// Start a unit in a given mode (e.g., "replace")
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

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

/// The systemd operations a deploy needs. `SystemdClient` implements it
/// over the system D-Bus; tests substitute a recording mock so the whole
/// deploy flow can run without systemd (and without touching real units).
#[allow(async_fn_in_trait)]
pub trait UnitManager {
    /// daemon-reload equivalent (re-runs generators).
    async fn daemon_reload(&self) -> Result<()>;
    /// Restart a unit and wait until systemd reports the job finished.
    async fn restart_unit_and_await(&self, unit_name: &str) -> Result<()>;
    /// Stop a unit and wait until systemd reports the job finished.
    async fn stop_unit_and_await(&self, unit_name: &str) -> Result<()>;
    /// Start a unit and wait until systemd reports the job finished.
    async fn start_unit_and_await(&self, unit_name: &str) -> Result<()>;
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
        std::fs::create_dir_all(&target_dir)
            .with_context(|| format!("creating units dir {}", target_dir.display()))?;

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

}

impl UnitManager for SystemdClient {
    /// Triggers systemd Reload() to execute generators and pick up unit file changes
    async fn daemon_reload(&self) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;
        proxy
            .reload()
            .await
            .context("D-Bus Reload() failed (polkit authorization missing for reload)")?;
        tracing::info!("systemd daemon-reload completed via D-Bus");
        Ok(())
    }

    /// Triggers unit restart and awaits the JobRemoved signal for completion confirmation
    async fn restart_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;

        // Subscribe to JobRemoved signals before triggering job to avoid missing fast completions
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = proxy
            .restart_unit(unit_name, "replace")
            .await
            .with_context(|| format!("D-Bus call RestartUnit for '{unit_name}' failed"))?;

        tracing::info!("started job {job_path} for unit '{unit_name}', awaiting JobRemoved (restart)...");

        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("unit '{unit_name}' restart completed successfully");
                    return Ok(());
                } else {
                    bail!(
                        "job for unit '{unit_name}' (restart) failed with result status: '{}'",
                        args.result
                    );
                }
            }
        }

        bail!("JobRemoved signal stream ended prematurely for '{unit_name}' (restart)");
    }

    /// Triggers unit stop and awaits the JobRemoved signal for completion confirmation.
    /// Used ahead of `start_unit_and_await` so that all units in a deploy are confirmed
    /// down before any of them come back up on the new release — two versions of the
    /// same program should never run at once.
    async fn stop_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;

        // Subscribe to JobRemoved signals before triggering job to avoid missing fast completions
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = proxy
            .stop_unit(unit_name, "replace")
            .await
            .with_context(|| format!("D-Bus call StopUnit for '{unit_name}' failed"))?;

        tracing::info!("started job {job_path} for unit '{unit_name}', awaiting JobRemoved (stop)...");

        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("unit '{unit_name}' stop completed successfully");
                    return Ok(());
                } else {
                    bail!(
                        "job for unit '{unit_name}' (stop) failed with result status: '{}'",
                        args.result
                    );
                }
            }
        }

        bail!("JobRemoved signal stream ended prematurely for '{unit_name}' (stop)");
    }

    /// Triggers unit start and awaits the JobRemoved signal for completion confirmation.
    async fn start_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;

        // Subscribe to JobRemoved signals before triggering job to avoid missing fast completions
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = proxy
            .start_unit(unit_name, "replace")
            .await
            .with_context(|| format!("D-Bus call StartUnit for '{unit_name}' failed"))?;

        tracing::info!("started job {job_path} for unit '{unit_name}', awaiting JobRemoved (start)...");

        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("unit '{unit_name}' start completed successfully");
                    return Ok(());
                } else {
                    bail!(
                        "job for unit '{unit_name}' (start) failed with result status: '{}'",
                        args.result
                    );
                }
            }
        }

        bail!("JobRemoved signal stream ended prematurely for '{unit_name}' (start)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deploy_common::manifest::UnitEntry;
    use std::fs;
    use tempfile::tempdir;

    fn unit(name: &str) -> UnitEntry {
        UnitEntry {
            name: name.into(),
            path: format!("systemd/{name}"),
            sha256: String::new(), // not checked by sync_unit_files
        }
    }

    fn write_release_unit(release: &Path, name: &str, content: &str) {
        fs::create_dir_all(release.join("systemd")).unwrap();
        fs::write(release.join("systemd").join(name), content).unwrap();
    }

    #[test]
    fn writes_new_units_and_reports_change() {
        let tmp = tempdir().unwrap();
        let (root, rel) = (tmp.path().join("cfg"), tmp.path().join("rel"));
        write_release_unit(&rel, "a.service", "A");
        write_release_unit(&rel, "b.timer", "B");

        let changed =
            SystemdClient::sync_unit_files(&root, "p", &rel, &[unit("a.service"), unit("b.timer")])
                .unwrap();

        assert!(changed);
        assert_eq!(fs::read_to_string(root.join("units/p/a.service")).unwrap(), "A");
        assert_eq!(fs::read_to_string(root.join("units/p/b.timer")).unwrap(), "B");
    }

    #[test]
    fn identical_content_reports_no_change() {
        let tmp = tempdir().unwrap();
        let (root, rel) = (tmp.path().join("cfg"), tmp.path().join("rel"));
        write_release_unit(&rel, "a.service", "A");
        let units = [unit("a.service")];
        assert!(SystemdClient::sync_unit_files(&root, "p", &rel, &units).unwrap());
        assert!(!SystemdClient::sync_unit_files(&root, "p", &rel, &units).unwrap());
    }

    #[test]
    fn modified_content_is_rewritten() {
        let tmp = tempdir().unwrap();
        let (root, rel) = (tmp.path().join("cfg"), tmp.path().join("rel"));
        write_release_unit(&rel, "a.service", "old");
        let units = [unit("a.service")];
        SystemdClient::sync_unit_files(&root, "p", &rel, &units).unwrap();
        write_release_unit(&rel, "a.service", "new");
        assert!(SystemdClient::sync_unit_files(&root, "p", &rel, &units).unwrap());
        assert_eq!(fs::read_to_string(root.join("units/p/a.service")).unwrap(), "new");
    }

    #[test]
    fn no_units_creates_dir_and_reports_no_change() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("cfg");
        assert!(!SystemdClient::sync_unit_files(&root, "p", tmp.path(), &[]).unwrap());
        assert!(root.join("units/p").is_dir());
    }

    #[test]
    fn missing_source_unit_fails() {
        let tmp = tempdir().unwrap();
        let err = SystemdClient::sync_unit_files(
            &tmp.path().join("cfg"),
            "p",
            &tmp.path().join("rel"),
            &[unit("a.service")],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("reading extracted unit"));
    }

    #[test]
    fn unwritable_units_root_fails() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("cfg");
        fs::write(&root, b"a file, not a dir").unwrap();
        assert!(SystemdClient::sync_unit_files(&root, "p", tmp.path(), &[]).is_err());
    }
}
