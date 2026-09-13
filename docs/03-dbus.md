# Step 3: D-Bus Integration via `zbus` — Systemd Operations & Unit Lifecycle

This guide covers wiring systemd unit lifecycle operations into `deploy-agent` using `zbus` to interact directly with the `org.freedesktop.systemd1` D-Bus interface.

Per the architecture defined in the project handoff, the agent operates as an unprivileged user. Systemd interaction relies on D-Bus methods gated by polkit policy, alongside a systemd generator to dynamically expose unit files dropped in agent-controlled paths.

---

**Step 3 Open Design Decisions**

The following design choices are left open for you to evaluate and decide during implementation:

* **Completion Signal Strategy**: Should the endpoint wait for D-Bus `JobRemoved` signals synchronously (blocking the deploy HTTP response until units reach `done` or `failed`), or trigger unit actions asynchronously and return immediately once D-Bus enqueues the job ID?
* **Failure Handling Strategy**: If a project defines multiple units (e.g., `api.service` and `worker.service`) and the second unit fails to restart during deploy, should the agent automatically execute a rollback (swapping the symlink back to the previous release and restarting the prior unit version), or exit with an error state and rely on explicit external health checks / manual rollback commands?
* **Generator Source Directory Structure**: Should unit template files be mirrored to `/etc/deploy-agent/units/<project>/<unit>.service` or nested under `/var/lib/deploy-agent/units/<project>/`?

---

**3.1 Workspace Updates**

Add `zbus` and `futures-util` to `deploy-agent/Cargo.toml`:

```toml
[dependencies]
# ... existing dependencies from Step 2 ...
zbus = { version = "4", features = ["tokio"] }
futures-util = "0.3"

```

---

**3.2 Unit Directory Layout & Generator Contract**

To deploy unit changes without root or `sudo`, `deploy-agent` writes unit files into an agent-owned directory structure:

```
/etc/deploy-agent/units/
  <project>/
    <unit>.service

```

A root-owned systemd generator (installed once out-of-band) symlinks these unit files into `/run/systemd/generator/` during daemon reloads.

Create `deploy-agent/src/systemd.rs` to isolate D-Bus interactions and generator path sync logic:

```rust
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use zbus::{dbus_proxy, zvariant::ObjectPath, Connection};

/// Client proxy for org.freedesktop.systemd1.Manager
#[dbus_proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    /// Start a unit in a given mode (e.g., "replace", "fail")
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<ObjectPath<'static>>;

    /// Stop a unit in a given mode
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<ObjectPath<'static>>;

    /// Restart a unit in a given mode
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<ObjectPath<'static>>;

    /// Reload systemd manager configuration (equivalent to daemon-reload)
    fn reload(&self) -> zbus::Result<()>;
}

pub struct SystemdClient {
    conn: Connection,
}

impl SystemdClient {
    /// Connects to the system D-Bus bus.
    pub async fn connect_system() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("failed to connect to D-Bus system bus")?;
        Ok(Self { conn })
    }

    /// Syncs unit files from an extracted release bundle into the agent's unit directory.
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

    /// Triggers systemd Reload() to run generators and refresh unit status.
    pub async fn daemon_reload(&self) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;
        proxy
            .reload()
            .await
            .context("D-Bus call Reload() failed (check polkit policy for Reload)")?;
        tracing::info!("systemd daemon-reload completed via D-Bus");
        Ok(())
    }

    /// Restarts a single unit and optionally awaits the JobRemoved signal.
    pub async fn restart_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;

        // Subscribe to JobRemoved signal stream prior to initiating request
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = proxy
            .restart_unit(unit_name, "replace")
            .await
            .with_context(|| format!("D-Bus call RestartUnit for '{unit_name}' failed"))?;

        tracing::info!("initiated restart for unit '{unit_name}', job path: {job_path}");

        // Monitor stream until matching job ID finishes
        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("job for unit '{unit_name}' completed successfully (result=done)");
                    return Ok(());
                } else {
                    bail!("job for unit '{unit_name}' failed with result: '{}'", args.result);
                }
            }
        }

        bail!("JobRemoved stream ended before unit '{unit_name}' completed");
    }
}

```

---

**3.3 Integrating D-Bus Unit Lifecycle into `main.rs**`

Update `deploy-agent/src/main.rs` to replace the stubbed systemd section from Step 2 with the actual `SystemdClient` workflow:

```rust
mod systemd;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use deploy_common::{bundle, hmac as dhmac, project::ProjectConfig, release::ReleaseLayout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use systemd::SystemdClient;

const REPLAY_WINDOW_SECS: i64 = 300;

#[derive(Clone)]
struct AppState {
    config_root: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config_root = std::env::var("DEPLOY_AGENT_CONFIG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/deploy-agent"));

    let state = Arc::new(AppState { config_root });

    let app = Router::new()
        .route("/deploy", post(handle_deploy))
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(state);

    let cert_path = std::env::var("DEPLOY_AGENT_TLS_CERT")
        .unwrap_or_else(|_| "/tmp/deploy-agent-test/cert.pem".into());
    let key_path = std::env::var("DEPLOY_AGENT_TLS_KEY")
        .unwrap_or_else(|_| "/tmp/deploy-agent-test/key.pem".into());

    if std::path::Path::new(&cert_path).exists() {
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
            .await
            .expect("loading TLS cert/key");
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8443));
        tracing::info!("listening on {addr} (TLS)");
        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    } else {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8443));
        tracing::info!("listening on {addr} (plain HTTP)");
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    }
}

async fn handle_deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    match do_deploy(&state, &headers, &body).await {
        Ok(release_id) => (StatusCode::OK, format!("deployed release {release_id}\n")),
        Err(e) => {
            tracing::warn!("deploy failed: {e:#}");
            (StatusCode::BAD_REQUEST, format!("deploy failed: {e:#}\n"))
        }
    }
}

async fn do_deploy(state: &AppState, headers: &HeaderMap, body: &[u8]) -> anyhow::Result<String> {
    let project = header_str(headers, "x-deploy-project")?;
    let timestamp: i64 = header_str(headers, "x-deploy-timestamp")?
        .parse()
        .map_err(|_| anyhow::anyhow!("X-Deploy-Timestamp invalid"))?;
    let signature_header = header_str(headers, "x-deploy-signature")?;
    let signature_hex = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow::anyhow!("X-Deploy-Signature missing 'sha256=' prefix"))?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    if (now - timestamp).abs() > REPLAY_WINDOW_SECS {
        anyhow::bail!("timestamp outside replay window");
    }

    let project_config_path = state.config_root.join("projects").join(format!("{project}.toml"));
    let project_config = ProjectConfig::load(&project_config_path)?;
    let secret = std::fs::read(&project_config.secret_path)?;

    dhmac::verify_signature(&secret, timestamp, project, body, signature_hex)?;

    let staging_dir = tempfile::tempdir()?;
    let bundle_path = staging_dir.path().join("bundle.tar.zst");
    std::fs::write(&bundle_path, body)?;

    let extract_dir = staging_dir.path().join("extracted");
    bundle::extract_bundle(&bundle_path, &extract_dir)?;

    let manifest = bundle::read_manifest(&extract_dir)?;
    if manifest.project != project {
        anyhow::bail!("manifest project mismatch");
    }
    bundle::verify_checksums(&extract_dir, &manifest)?;

    // Sync unit files to /etc/deploy-agent/units/<project>/
    let units_changed = SystemdClient::sync_unit_files(
        &state.config_root,
        project,
        &extract_dir,
        &manifest.units,
    )?;

    // Perform atomic release symlink swap
    let layout = ReleaseLayout::new(&project_config.install_dir);
    std::fs::create_dir_all(layout.releases_dir())?;
    let release_path = layout.new_release_path(manifest.built_at, &manifest.git_sha);
    if release_path.exists() {
        anyhow::bail!("release dir already exists");
    }
    move_dir(&extract_dir, &release_path)?;
    layout.swap_current(&release_path)?;

    // Connect D-Bus and manage systemd units
    let dbus_client = SystemdClient::connect_system().await?;

    if units_changed {
        tracing::info!("unit files updated; invoking daemon-reload");
        dbus_client.daemon_reload().await?;
    }

    for unit in &manifest.units {
        if project_config.allowed_units.contains(&unit.name) {
            tracing::info!("restarting allowed unit: {}", unit.name);
            dbus_client.restart_unit_and_await(&unit.name).await?;
        } else {
            tracing::warn!("unit '{}' in manifest rejected by project allow-list", unit.name);
        }
    }

    let release_id = release_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(release_id)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> anyhow::Result<&'a str> {
    headers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("missing required header: {name}"))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("header {name} is not valid UTF-8"))
}

fn move_dir(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_recursive(from, to)?;
            std::fs::remove_dir_all(from)?;
            Ok(())
        }
    }
}

fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

```

---

**3.4 Systemd Generator Setup**

Create the generator script at `/etc/systemd/system-generators/deploy-agent-generator` (owned by `root:root`, executable `0755`):

```bash
#!/bin/bash
# /etc/systemd/system-generators/deploy-agent-generator
# Generates symlinks in systemd runtime search path for deploy-agent units.

NORMAL_DIR="$1"
UNITS_DIR="/etc/deploy-agent/units"

if [ -d "$UNITS_DIR" ]; then
    find "$UNITS_DIR" -type f -name "*.service" | while read -r unit_file; do
        filename=$(basename "$unit_file")
        ln -sfn "$unit_file" "$NORMAL_DIR/$filename"
    done
fi

```

---

**3.5 Verification & Step Exit Criteria**

Before proceeding to **Step 4: Generator & Polkit Verification**:

* [ ] `cargo check -p deploy-agent` builds cleanly with `zbus` dependencies.
* [ ] Running `deploy-agent` on a machine with D-Bus triggers unit restart attempts over the system bus when sending a signed bundle.
* [ ] D-Bus `JobRemoved` signal processing captures state transitions correctly (`done` vs `failed`).
* [ ] Unit files extracted from bundles are correctly synchronized under `/etc/deploy-agent/units/<project>/`.

Ready to proceed to **Step 4: Polkit Authorization Rules & Generator Integration**?
