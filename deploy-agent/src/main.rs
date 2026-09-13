use anyhow::bail;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use deploy_common::{bundle, hmac as dhmac, project::ProjectConfig, release::ReleaseLayout};
use tracing::debug;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::systemd::SystemdClient;

mod systemd;

const REPLAY_WINDOW_SECS: i64 = 300; // 5 minutes, per the handoff

#[derive(Clone)]
struct AppState {
    /// Root directory containing projects/<name>.toml. Defaults to
    /// /etc/deploy-agent, but overridable via DEPLOY_AGENT_CONFIG_ROOT so
    /// this step can be tested entirely as a non-root user against a
    /// scratch directory before anything touches real /etc paths.
    config_root: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config_root = std::env::var("DEPLOY_AGENT_CONFIG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/deploy-agent"));

    // Self-prune on startup: Clean up old releases of deploy-agent left behind by previous self-updates
    prune_self_on_startup(&config_root);

    let state = Arc::new(AppState { config_root });

    let app = Router::new()
        .route("/deploy", post(handle_deploy))
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(state);

    let cert_path = std::env::var("DEPLOY_AGENT_TLS_CERT")
        .unwrap_or_else(|_| "/tmp/deploy-agent-test/cert.pem".into());
    let key_path = std::env::var("DEPLOY_AGENT_TLS_KEY")
        .unwrap_or_else(|_| "/tmp/deploy-agent-test/key.pem".into());
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .expect("loading TLS cert/key");

    let addr: std::net::SocketAddr = std::env::var("DEPLOY_AGENT_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8443".to_string())
        .parse()
        .expect("DEPLOY_AGENT_BIND_ADDR must be a valid host:port socket address");
    tracing::info!("listening on {addr} (TLS)");
    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

/// Prunes old deploy-agent releases when starting up
fn prune_self_on_startup(config_root: &Path) {
    let own_project_name =
        std::env::var("DEPLOY_AGENT_PROJECT_NAME").unwrap_or_else(|_| "deploy-agent".to_string());

    let project_config_path = config_root
        .join("projects")
        .join(format!("{own_project_name}.toml"));
    if let Ok(config) = ProjectConfig::load(&project_config_path) {
        let layout = ReleaseLayout::new(&config.install_dir);
        let retain_count = config.retain_count;
        if let Err(e) = layout.prune(retain_count) {
            tracing::warn!("failed startup self-prune for project '{own_project_name}': {e:#}");
        } else {
            tracing::info!(
                "startup self-prune completed for '{own_project_name}' (retain_count={retain_count})"
            );
        }
    }
}

async fn handle_deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    match do_deploy(&state, &headers, &body).await {
        Ok((release_id, is_self_update)) => {
            if is_self_update {
                tracing::info!(
                    "self-update detected: scheduling agent exit for systemd restart..."
                );
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    tracing::info!("exiting process for self-update restart");
                    std::process::exit(0);
                });
                (
                    StatusCode::OK,
                    format!("deployed self-update release {release_id}; agent restarting...\n"),
                )
            } else {
                (StatusCode::OK, format!("deployed release {release_id}\n"))
            }
        }
        Err(e) => {
            tracing::error!("deploy failed: {e:#}");
            (StatusCode::BAD_REQUEST, format!("deploy failed: {e:#}\n"))
        }
    }
}

async fn do_deploy(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> anyhow::Result<(String, bool)> {
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

    let project_config_path = state
        .config_root
        .join("projects")
        .join(format!("{project}.toml"));
    if !project_config_path.exists() {
        bail!("Project {project} is not configured.")
    }
    let project_config = ProjectConfig::load(&project_config_path)?;
    if !project_config.secret_path.exists() {
        debug!("Secret at {}", project_config.secret_path.display());
        bail!("Secret for project {project} is not found.")
    }
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
    if let Some(u) = manifest.units.iter().find(|u| !project_config.allowed_units.contains(&u.name)) {
        anyhow::bail!("manifest unit {} was not declared in the project", u.name);
    }
    bundle::verify_checksums(&extract_dir, &manifest)?;


    let layout = ReleaseLayout::new(&project_config.install_dir);
    std::fs::create_dir_all(layout.releases_dir())?;

    // Track previous release target for potential rollback
    let previous_release = layout.current_target()?;

    let release_path = layout.new_release_path(manifest.built_at, &manifest.git_sha);
    if release_path.exists() {
        anyhow::bail!("release dir {} already exists", release_path.display());
    }

    move_dir(&extract_dir, &release_path)?;

    // Sync unit files
    let units_changed = SystemdClient::sync_unit_files(
        &state.config_root,
        project,
        &release_path,
        &manifest.units,
    )?;

    // Atomic symlink swap to new release
    layout.swap_current(&release_path)?;

    let own_unit_name = std::env::var("DEPLOY_AGENT_UNIT_NAME")
        .unwrap_or_else(|_| "deploy-agent.service".to_string());

    let mut is_self_update = false;

    // Connect D-Bus and manage systemd units
    let dbus_client = match SystemdClient::connect_system().await {
        Ok(c) => c,
        Err(e) => {
            rollback(&layout, previous_release.as_deref()).ok();
            return Err(e);
        }
    };

    // Split units into "self" (deploy-agent can't stop itself and then call
    // StartUnit on itself, so its restart is deferred to process exit and
    // systemd's own restart policy) and everything else, which we manage
    // directly via D-Bus.
    let mut units_to_manage: Vec<&str> = Vec::new();
    for unit in &manifest.units {
        if unit.name == own_unit_name {
            tracing::info!(
                "manifest contains self-unit '{}'; deferring restart to exit",
                unit.name
            );
            is_self_update = true;
            continue;
        }

        if project_config.allowed_units.contains(&unit.name) {
            units_to_manage.push(unit.name.as_str());
        } else {
            tracing::warn!("unit '{}' skipped (not on allow-list)", unit.name);
        }
    }

    // Step 1: stop every managed unit and wait for it to actually be down.
    // Some deploys run multiple cooperating programs that apply migrations
    // on startup, and having the old and new version running concurrently
    // can cause conflicts — so nothing gets reloaded or started until the
    // old processes are confirmed gone.
    for unit_name in &units_to_manage {
        if let Err(e) = dbus_client.stop_unit_and_await(unit_name).await {
            tracing::error!(
                "unit '{unit_name}' failed to stop cleanly, initiating automated rollback..."
            );
            return match perform_full_rollback(
                &layout,
                previous_release.as_deref(),
                &dbus_client,
                &project_config,
            )
            .await
            {
                Ok(_) => Err(anyhow::anyhow!(
                    "unit '{unit_name}' failed to stop: {e:#}. Rollback successful."
                )),
                Err(rb_err) => Err(anyhow::anyhow!(
                    "unit '{unit_name}' failed to stop: {e:#}. ROLLBACK FAILED: {rb_err:#}"
                )),
            };
        }
    }

    // Step 2: reload unit files now that nothing from the old release is
    // still running, so the reload can't race a live process.
    if units_changed && let Err(e) = dbus_client.daemon_reload().await {
        tracing::error!("daemon-reload failed after stopping units, initiating automated rollback...");
        return match perform_full_rollback(
            &layout,
            previous_release.as_deref(),
            &dbus_client,
            &project_config,
        )
        .await
        {
            Ok(_) => Err(anyhow::anyhow!(
                "daemon-reload failed: {e:#}. Rollback successful."
            )),
            Err(rb_err) => Err(anyhow::anyhow!(
                "daemon-reload failed: {e:#}. ROLLBACK FAILED: {rb_err:#}"
            )),
        };
    }

    // Step 3: start everything on the new release. (Not fully awaiting
    // service readiness beyond systemd's own start-up wait, for now.)
    for unit_name in &units_to_manage {
        if let Err(e) = dbus_client.start_unit_and_await(unit_name).await {
            tracing::error!(
                "unit '{unit_name}' failed to start, initiating automated rollback..."
            );
            return match perform_full_rollback(
                &layout,
                previous_release.as_deref(),
                &dbus_client,
                &project_config,
            )
            .await
            {
                Ok(_) => Err(anyhow::anyhow!(
                    "unit '{unit_name}' failed to start: {e:#}. Rollback successful."
                )),
                Err(rb_err) => Err(anyhow::anyhow!(
                    "unit '{unit_name}' failed to start: {e:#}. ROLLBACK FAILED: {rb_err:#}"
                )),
            };
        }
    }

    // Prune old releases for standard deployments (skip if self-update)
    if !is_self_update {
        let retain_count = project_config.retain_count;
        if let Err(e) = layout.prune(retain_count) {
            tracing::warn!("release prune failed for '{project}': {e:#}");
        } else {
            tracing::info!("pruned old releases for '{project}' (retain_count={retain_count})");
        }
    }

    let release_id = release_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok((release_id, is_self_update))
}

fn rollback(layout: &ReleaseLayout, previous: Option<&Path>) -> anyhow::Result<()> {
    if let Some(prev) = previous {
        layout.swap_current(prev)?;
        tracing::info!("rolled back 'current' symlink to {}", prev.display());
    }
    Ok(())
}

async fn perform_full_rollback(
    layout: &ReleaseLayout,
    previous: Option<&Path>,
    dbus: &SystemdClient,
    config: &ProjectConfig,
) -> anyhow::Result<()> {
    rollback(layout, previous)?;

    // Restart allowed units on previous release
    for unit_name in &config.allowed_units {
        dbus.restart_unit_and_await(unit_name).await?;
    }
    Ok(())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> anyhow::Result<&'a str> {
    headers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("missing required header: {name}"))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("header {name} is not valid UTF-8"))
}

fn move_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
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

fn copy_dir_recursive(from: &Path, to: &Path) -> anyhow::Result<()> {
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
