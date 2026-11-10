use anyhow::{bail, Context};
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

use crate::systemd::{SystemdClient, UnitManager};

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

    // Pick the TLS crypto backend explicitly. rustls only auto-selects when
    // exactly one of ring / aws-lc-rs is compiled in, and building the whole
    // workspace together turns on both (deploy-ci's ureq brings ring).
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("installing rustls CryptoProvider");

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

    // Exit cleanly on SIGTERM (systemd stop) / SIGINT, letting in-flight
    // deploys finish instead of dying mid-swap.
    let handle = axum_server::Handle::new();
    tokio::spawn(shutdown_on_signal(handle.clone()));
    axum_server::bind_rustls(addr, tls_config)
        .handle(handle)
        .serve(app.into_make_service())
        .await
        .unwrap();
    tracing::info!("shut down");
}

async fn shutdown_on_signal(handle: axum_server::Handle<std::net::SocketAddr>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("installing SIGINT handler");
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
        _ = int.recv() => tracing::info!("SIGINT received, shutting down"),
    }
    handle.graceful_shutdown(Some(Duration::from_secs(30)));
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
    match do_deploy(&state, &headers, &body, SystemdClient::connect_system()).await {
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
            // Full context chain (which path, which syscall) goes to the
            // journal only — the HTTP response stays terse and stays clear
            // of internal filesystem layout, even though the caller already
            // holds a valid HMAC key for this project.
            tracing::error!("deploy failed: {e:#}");
            (StatusCode::BAD_REQUEST, format!("deploy failed: {e}\n"))
        }
    }
}

/// Runs one deploy. `connect` yields the systemd handle; it is only awaited
/// once the new release is verified and swapped in, so every rejection path
/// returns before any systemd interaction. Production passes
/// `SystemdClient::connect_system()`; tests pass a mock.
async fn do_deploy<M, C>(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    connect: C,
) -> anyhow::Result<(String, bool)>
where
    M: UnitManager,
    C: std::future::Future<Output = anyhow::Result<M>>,
{
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
    let secret = std::fs::read(&project_config.secret_path)
        .with_context(|| format!("reading secret {}", project_config.secret_path.display()))?;

    dhmac::verify_signature(&secret, timestamp, project, body, signature_hex)?;

    let staging_dir = tempfile::tempdir().context("creating staging temp dir")?;
    let bundle_path = staging_dir.path().join("bundle.tar.zst");
    std::fs::write(&bundle_path, body)
        .with_context(|| format!("writing received bundle to {}", bundle_path.display()))?;

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
    std::fs::create_dir_all(layout.releases_dir())
        .with_context(|| format!("creating releases dir {}", layout.releases_dir().display()))?;

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

    // Only talk to systemd when there is something for it to do: a bundle
    // with no units (or only the agent's own, unchanged unit) deploys fine
    // on a host without a reachable bus.
    if units_changed || !units_to_manage.is_empty() {
        // Connect D-Bus and manage systemd units
        let dbus_client = match connect.await {
            Ok(c) => c,
            Err(e) => {
                rollback(&layout, previous_release.as_deref()).ok();
                return Err(e);
            }
        };

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
    dbus: &impl UnitManager,
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
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        // rename() fails across filesystem boundaries (e.g. staging in
        // /tmp, release dir on a separately-mounted app volume) — fall back
        // to copy+remove rather than treating every rename() error as fatal.
        Err(_) => {
            copy_dir_recursive(from, to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            std::fs::remove_dir_all(from)
                .with_context(|| format!("removing staged dir {}", from.display()))?;
            Ok(())
        }
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading dir entry in {}", from.display()))?;
        let dest = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat-ing {}", entry.path().display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest).with_context(|| {
                format!("copying {} to {}", entry.path().display(), dest.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! `do_deploy` is driven end to end against a `MockSystemd` that
    //! records every systemd call and can be told to fail specific ones,
    //! so these tests behave the same with or without systemd and never
    //! touch real units.
    use super::*;
    use axum::http::HeaderValue;
    use deploy_common::manifest::{BinaryEntry, CURRENT_SCHEMA_VERSION, Manifest, UnitEntry};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    const PROJECT: &str = "myproj";
    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const UNIT: &str = "myproj.service";
    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // ---------------------------------------------------------------
    // Mock systemd
    // ---------------------------------------------------------------

    #[derive(Default)]
    struct MockInner {
        calls: Mutex<Vec<String>>,
        fail_stop: Option<String>,
        fail_start: Option<String>,
        fail_restart: bool,
        fail_reload: bool,
    }

    /// Cheap to clone; clones share the same call log.
    #[derive(Clone, Default)]
    struct MockSystemd(Arc<MockInner>);

    impl MockSystemd {
        fn with(f: impl FnOnce(&mut MockInner)) -> Self {
            let mut inner = MockInner::default();
            f(&mut inner);
            Self(Arc::new(inner))
        }
        fn calls(&self) -> Vec<String> {
            self.0.calls.lock().unwrap().clone()
        }
        fn log(&self, call: String) {
            self.0.calls.lock().unwrap().push(call);
        }
        /// Connector future handed to `do_deploy`.
        async fn connect(self) -> anyhow::Result<Self> {
            Ok(self)
        }
    }

    impl UnitManager for MockSystemd {
        async fn daemon_reload(&self) -> anyhow::Result<()> {
            self.log("reload".into());
            if self.0.fail_reload {
                bail!("mock: reload failed");
            }
            Ok(())
        }
        async fn restart_unit_and_await(&self, unit: &str) -> anyhow::Result<()> {
            self.log(format!("restart {unit}"));
            if self.0.fail_restart {
                bail!("mock: restart failed");
            }
            Ok(())
        }
        async fn stop_unit_and_await(&self, unit: &str) -> anyhow::Result<()> {
            self.log(format!("stop {unit}"));
            if self.0.fail_stop.as_deref() == Some(unit) {
                bail!("mock: stop failed");
            }
            Ok(())
        }
        async fn start_unit_and_await(&self, unit: &str) -> anyhow::Result<()> {
            self.log(format!("start {unit}"));
            if self.0.fail_start.as_deref() == Some(unit) {
                bail!("mock: start failed");
            }
            Ok(())
        }
    }

    // ---------------------------------------------------------------
    // Fixture: config root + project config + secret + bundle builder
    // ---------------------------------------------------------------

    struct Fixture {
        tmp: TempDir,
        state: AppState,
        install_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_project(PROJECT, &[UNIT], 5)
        }

        fn with_project(project: &str, allowed: &[&str], retain_count: usize) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let config_root = tmp.path().join("etc");
            let install_dir = tmp.path().join("app");
            fs::create_dir_all(config_root.join("projects")).unwrap();
            fs::create_dir_all(config_root.join("secrets")).unwrap();
            let secret_path = config_root.join(format!("secrets/{project}.key"));
            fs::write(&secret_path, SECRET).unwrap();
            let cfg = ProjectConfig {
                install_dir: install_dir.clone(),
                allowed_units: allowed.iter().map(|s| s.to_string()).collect(),
                retain_count,
                health_check: None,
                secret_path,
            };
            fs::write(
                config_root.join(format!("projects/{project}.toml")),
                toml::to_string(&cfg).unwrap(),
            )
            .unwrap();
            Self { state: AppState { config_root }, install_dir, tmp }
        }

        fn layout(&self) -> ReleaseLayout {
            ReleaseLayout::new(&self.install_dir)
        }

        fn units_dir(&self) -> PathBuf {
            self.state.config_root.join("units").join(PROJECT)
        }

        fn bundle(&self, spec: Spec) -> (Vec<u8>, Manifest) {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = self.tmp.path().join(format!("staging{}", N.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(dir.join("bin")).unwrap();
            fs::create_dir_all(dir.join("systemd")).unwrap();
            let bin = b"#!/bin/true\n";
            fs::write(dir.join("bin/app"), bin).unwrap();
            let mut units = Vec::new();
            for name in spec.units {
                fs::write(dir.join("systemd").join(name), spec.unit_content).unwrap();
                units.push(UnitEntry {
                    name: name.to_string(),
                    path: format!("systemd/{name}"),
                    sha256: sha(spec.unit_content.as_bytes()),
                });
            }
            let manifest = Manifest {
                schema_version: CURRENT_SCHEMA_VERSION,
                project: spec.project.into(),
                git_sha: spec.git_sha.into(),
                built_at: time::OffsetDateTime::now_utc(),
                build_meta: BTreeMap::new(),
                binaries: vec![BinaryEntry {
                    path: "bin/app".into(),
                    sha256: if spec.corrupt_binary_sha { sha(b"other") } else { sha(bin) },
                    mode: 0o755,
                }],
                units,
            };
            fs::write(dir.join("manifest.toml"), toml::to_string_pretty(&manifest).unwrap())
                .unwrap();
            let out = dir.with_extension("tar.zst");
            bundle::build_bundle(&dir, &out).unwrap();
            (fs::read(&out).unwrap(), manifest)
        }

        /// Builds, signs and deploys a bundle through `mock`.
        async fn deploy(&self, spec: Spec<'_>, mock: &MockSystemd) -> anyhow::Result<(String, bool)> {
            let project = spec.project;
            let (body, _) = self.bundle(spec);
            let h = signed_headers(project, now(), SECRET, &body);
            do_deploy(&self.state, &h, &body, mock.clone().connect()).await
        }

        /// Asserts a rejection happened before anything was touched.
        async fn reject(&self, headers: &HeaderMap, body: &[u8]) -> String {
            let mock = MockSystemd::default();
            let err = do_deploy(&self.state, headers, body, mock.clone().connect())
                .await
                .unwrap_err();
            assert!(mock.calls().is_empty(), "systemd touched on rejection: {:?}", mock.calls());
            assert!(fs::symlink_metadata(self.install_dir.join("current")).is_err());
            assert!(!self.state.config_root.join("units").exists());
            format!("{err:#}")
        }
    }

    #[derive(Clone, Copy)]
    struct Spec<'a> {
        project: &'a str,
        git_sha: &'a str,
        units: &'a [&'a str],
        unit_content: &'a str,
        corrupt_binary_sha: bool,
    }

    impl Default for Spec<'_> {
        fn default() -> Self {
            Self {
                project: PROJECT,
                git_sha: SHA_A,
                units: &[UNIT],
                unit_content: "[Service]\nExecStart=/bin/true\n",
                corrupt_binary_sha: false,
            }
        }
    }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn signed_headers(project: &str, ts: i64, secret: &[u8], body: &[u8]) -> HeaderMap {
        let sig = dhmac::compute_signature(secret, ts, project, body).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-deploy-project", HeaderValue::from_str(project).unwrap());
        h.insert("x-deploy-timestamp", HeaderValue::from_str(&ts.to_string()).unwrap());
        h.insert("x-deploy-signature", HeaderValue::from_str(&format!("sha256={sig}")).unwrap());
        h
    }

    fn release_name(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    // ---------------------------------------------------------------
    // Happy paths
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn first_deploy_swaps_syncs_and_restarts() {
        let fx = Fixture::new();
        let mock = MockSystemd::default();

        let (release_id, is_self) = fx.deploy(Spec::default(), &mock).await.unwrap();

        assert!(!is_self);
        assert!(release_id.ends_with("_aaaaaaaaaaaa"), "{release_id}");
        let current = fx.layout().current_target().unwrap().unwrap();
        assert_eq!(release_name(&current), release_id);
        assert!(current.join("bin/app").is_file());
        assert!(current.join("manifest.toml").is_file());
        assert!(fx.units_dir().join(UNIT).is_file());
        assert_eq!(mock.calls(), [format!("stop {UNIT}"), "reload".into(), format!("start {UNIT}")]);
    }

    #[tokio::test]
    async fn redeploy_with_unchanged_units_skips_reload() {
        let fx = Fixture::new();
        fx.deploy(Spec::default(), &MockSystemd::default()).await.unwrap();

        let mock = MockSystemd::default();
        let (id, _) = fx.deploy(Spec { git_sha: SHA_B, ..Default::default() }, &mock).await.unwrap();

        assert!(id.ends_with("_bbbbbbbbbbbb"));
        assert_eq!(mock.calls(), [format!("stop {UNIT}"), format!("start {UNIT}")]);
        assert_eq!(fx.layout().list_releases().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn redeploy_with_changed_unit_reloads() {
        let fx = Fixture::new();
        fx.deploy(Spec::default(), &MockSystemd::default()).await.unwrap();

        let mock = MockSystemd::default();
        let spec = Spec { git_sha: SHA_B, unit_content: "[Service]\nExecStart=/bin/false\n", ..Default::default() };
        fx.deploy(spec, &mock).await.unwrap();

        assert!(mock.calls().contains(&"reload".to_string()));
        assert_eq!(
            fs::read_to_string(fx.units_dir().join(UNIT)).unwrap(),
            "[Service]\nExecStart=/bin/false\n"
        );
    }

    #[tokio::test]
    async fn multiple_units_are_all_stopped_before_any_start() {
        let fx = Fixture::with_project(PROJECT, &["a.service", "b.service"], 5);
        let mock = MockSystemd::default();
        fx.deploy(Spec { units: &["a.service", "b.service"], ..Default::default() }, &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        let last_stop = calls.iter().rposition(|c| c.starts_with("stop ")).unwrap();
        let first_start = calls.iter().position(|c| c.starts_with("start ")).unwrap();
        assert!(last_stop < first_start, "{calls:?}");
        assert_eq!(calls.iter().filter(|c| c.starts_with("stop ")).count(), 2);
        assert_eq!(calls.iter().filter(|c| c.starts_with("start ")).count(), 2);
    }

    #[tokio::test]
    async fn bundle_without_units_touches_no_units() {
        let fx = Fixture::new();
        let mock = MockSystemd::default();
        fx.deploy(Spec { units: &[], ..Default::default() }, &mock).await.unwrap();
        assert!(mock.calls().is_empty());
        assert!(fx.layout().current_target().unwrap().is_some());
    }

    #[tokio::test]
    async fn successful_deploy_prunes_to_retain_count() {
        let fx = Fixture::with_project(PROJECT, &[UNIT], 1);
        fx.deploy(Spec::default(), &MockSystemd::default()).await.unwrap();
        let (id, _) = fx
            .deploy(Spec { git_sha: SHA_B, ..Default::default() }, &MockSystemd::default())
            .await
            .unwrap();

        let releases = fx.layout().list_releases().unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(release_name(&releases[0]), id);
    }

    #[tokio::test]
    async fn self_update_defers_own_restart_and_skips_prune() {
        // Assumes DEPLOY_AGENT_UNIT_NAME is unset (default deploy-agent.service).
        let fx = Fixture::with_project(PROJECT, &["deploy-agent.service"], 1);
        let spec = Spec { units: &["deploy-agent.service"], ..Default::default() };
        fx.deploy(spec, &MockSystemd::default()).await.unwrap();

        let mock = MockSystemd::default();
        let (id, is_self) = fx.deploy(Spec { git_sha: SHA_B, ..spec }, &mock).await.unwrap();

        assert!(is_self);
        // Unit content is unchanged, so no reload; and the agent never
        // stops/starts itself over D-Bus.
        assert!(mock.calls().is_empty(), "{:?}", mock.calls());
        assert_eq!(release_name(&fx.layout().current_target().unwrap().unwrap()), id);
        // retain_count = 1, but self-updates leave pruning to startup.
        assert_eq!(fx.layout().list_releases().unwrap().len(), 2);
    }

    // ---------------------------------------------------------------
    // Failures after the swap -> rollback
    // ---------------------------------------------------------------

    /// Deploys release A successfully, returns its path.
    async fn deploy_baseline(fx: &Fixture) -> PathBuf {
        fx.deploy(Spec::default(), &MockSystemd::default()).await.unwrap();
        fx.layout().current_target().unwrap().unwrap()
    }

    #[tokio::test]
    async fn stop_failure_rolls_back() {
        let fx = Fixture::new();
        let previous = deploy_baseline(&fx).await;
        let mock = MockSystemd::with(|m| m.fail_stop = Some(UNIT.into()));

        let err = fx.deploy(Spec { git_sha: SHA_B, ..Default::default() }, &mock).await.unwrap_err();

        assert!(err.to_string().contains("failed to stop"), "{err}");
        assert!(err.to_string().contains("Rollback successful"));
        assert_eq!(fx.layout().current_target().unwrap(), Some(previous));
        assert_eq!(mock.calls(), [format!("stop {UNIT}"), format!("restart {UNIT}")]);
    }

    #[tokio::test]
    async fn start_failure_rolls_back() {
        let fx = Fixture::new();
        let previous = deploy_baseline(&fx).await;
        let mock = MockSystemd::with(|m| m.fail_start = Some(UNIT.into()));

        let err = fx.deploy(Spec { git_sha: SHA_B, ..Default::default() }, &mock).await.unwrap_err();

        assert!(err.to_string().contains("failed to start"), "{err}");
        assert!(err.to_string().contains("Rollback successful"));
        assert_eq!(fx.layout().current_target().unwrap(), Some(previous));
        assert_eq!(
            mock.calls(),
            [format!("stop {UNIT}"), format!("start {UNIT}"), format!("restart {UNIT}")]
        );
        // The failed release is left on disk (pruned by a later deploy).
        assert_eq!(fx.layout().list_releases().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reload_failure_rolls_back() {
        let fx = Fixture::new();
        let previous = deploy_baseline(&fx).await;
        let mock = MockSystemd::with(|m| m.fail_reload = true);
        let spec = Spec { git_sha: SHA_B, unit_content: "[Service]\nchanged\n", ..Default::default() };

        let err = fx.deploy(spec, &mock).await.unwrap_err();

        assert!(err.to_string().contains("daemon-reload failed"), "{err}");
        assert_eq!(fx.layout().current_target().unwrap(), Some(previous));
        assert_eq!(mock.calls(), [format!("stop {UNIT}"), "reload".into(), format!("restart {UNIT}")]);
    }

    #[tokio::test]
    async fn failed_rollback_is_reported() {
        let fx = Fixture::new();
        deploy_baseline(&fx).await;
        let mock = MockSystemd::with(|m| {
            m.fail_start = Some(UNIT.into());
            m.fail_restart = true;
        });

        let err = fx.deploy(Spec { git_sha: SHA_B, ..Default::default() }, &mock).await.unwrap_err();

        assert!(err.to_string().contains("ROLLBACK FAILED"), "{err}");
    }

    #[tokio::test]
    async fn rollback_restarts_all_allowed_units() {
        let fx = Fixture::with_project(PROJECT, &["a.service", "b.service"], 5);
        let spec = Spec { units: &["a.service"], ..Default::default() };
        fx.deploy(spec, &MockSystemd::default()).await.unwrap();
        let mock = MockSystemd::with(|m| m.fail_start = Some("a.service".into()));

        fx.deploy(Spec { git_sha: SHA_B, ..spec }, &mock).await.unwrap_err();

        let calls = mock.calls();
        assert!(calls.contains(&"restart a.service".to_string()));
        assert!(calls.contains(&"restart b.service".to_string()), "{calls:?}");
    }

    #[tokio::test]
    async fn rollback_leaves_new_unit_files_in_place() {
        // Documents current behaviour: unit files aren't restored.
        let fx = Fixture::new();
        deploy_baseline(&fx).await;
        let mock = MockSystemd::with(|m| m.fail_start = Some(UNIT.into()));
        let spec = Spec { git_sha: SHA_B, unit_content: "[Service]\nnew\n", ..Default::default() };

        fx.deploy(spec, &mock).await.unwrap_err();

        assert_eq!(fs::read_to_string(fx.units_dir().join(UNIT)).unwrap(), "[Service]\nnew\n");
    }

    #[tokio::test]
    async fn first_deploy_failure_has_nothing_to_roll_back_to() {
        let fx = Fixture::new();
        let mock = MockSystemd::with(|m| m.fail_start = Some(UNIT.into()));

        let err = fx.deploy(Spec::default(), &mock).await.unwrap_err();

        assert!(err.to_string().contains("Rollback successful"));
        // No previous release: current still points at the failed one.
        assert!(fx.layout().current_target().unwrap().is_some());
    }

    #[tokio::test]
    async fn connect_failure_swaps_back_without_touching_units() {
        let fx = Fixture::new();
        let previous = deploy_baseline(&fx).await;
        let (body, _) = fx.bundle(Spec { git_sha: SHA_B, ..Default::default() });
        let h = signed_headers(PROJECT, now(), SECRET, &body);

        let err = do_deploy(&fx.state, &h, &body, async {
            Err::<MockSystemd, _>(anyhow::anyhow!("mock: no system bus"))
        })
        .await
        .unwrap_err();

        assert!(err.to_string().contains("no system bus"));
        assert_eq!(fx.layout().current_target().unwrap(), Some(previous));
    }

    #[tokio::test]
    async fn no_systemd_work_never_connects() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec { units: &[], ..Default::default() });
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        do_deploy(&fx.state, &h, &body, async {
            Err::<MockSystemd, _>(anyhow::anyhow!("connect must not be awaited"))
        })
        .await
        .unwrap();
        assert!(fx.layout().current_target().unwrap().is_some());
    }

    // ---------------------------------------------------------------
    // Rejections before anything is touched
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn rejects_missing_headers() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec::default());
        let full = signed_headers(PROJECT, now(), SECRET, &body);
        for name in ["x-deploy-project", "x-deploy-timestamp", "x-deploy-signature"] {
            let mut h = full.clone();
            h.remove(name);
            let err = fx.reject(&h, &body).await;
            assert!(err.contains(&format!("missing required header: {name}")), "{err}");
        }
    }

    #[tokio::test]
    async fn rejects_non_numeric_timestamp() {
        let fx = Fixture::new();
        let mut h = signed_headers(PROJECT, now(), SECRET, b"x");
        h.insert("x-deploy-timestamp", HeaderValue::from_static("soon"));
        assert!(fx.reject(&h, b"x").await.contains("X-Deploy-Timestamp invalid"));
    }

    #[tokio::test]
    async fn rejects_signature_without_prefix() {
        let fx = Fixture::new();
        let mut h = signed_headers(PROJECT, now(), SECRET, b"x");
        h.insert("x-deploy-signature", HeaderValue::from_static("abcdef"));
        assert!(fx.reject(&h, b"x").await.contains("missing 'sha256=' prefix"));
    }

    #[tokio::test]
    async fn rejects_timestamps_outside_replay_window() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec::default());
        for ts in [now() - REPLAY_WINDOW_SECS - 5, now() + REPLAY_WINDOW_SECS + 5] {
            let h = signed_headers(PROJECT, ts, SECRET, &body);
            assert!(fx.reject(&h, &body).await.contains("replay window"));
        }
    }

    #[tokio::test]
    async fn accepts_timestamp_inside_replay_window() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec::default());
        let h = signed_headers(PROJECT, now() - REPLAY_WINDOW_SECS + 30, SECRET, &body);
        do_deploy(&fx.state, &h, &body, MockSystemd::default().connect()).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_unconfigured_project() {
        let fx = Fixture::new();
        let h = signed_headers("ghost", now(), SECRET, b"x");
        assert!(fx.reject(&h, b"x").await.contains("Project ghost is not configured"));
    }

    #[tokio::test]
    async fn rejects_when_secret_file_missing() {
        let fx = Fixture::new();
        fs::remove_file(fx.state.config_root.join("secrets/myproj.key")).unwrap();
        let h = signed_headers(PROJECT, now(), SECRET, b"x");
        assert!(fx.reject(&h, b"x").await.contains("Secret for project myproj is not found"));
    }

    #[tokio::test]
    async fn rejects_wrong_secret() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec::default());
        let h = signed_headers(PROJECT, now(), b"not-the-secret", &body);
        assert!(fx.reject(&h, &body).await.contains("signature verification failed"));
    }

    #[tokio::test]
    async fn rejects_tampered_body() {
        let fx = Fixture::new();
        let (mut body, _) = fx.bundle(Spec::default());
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        body.push(0);
        assert!(fx.reject(&h, &body).await.contains("signature verification failed"));
    }

    #[tokio::test]
    async fn rejects_garbage_body_with_valid_signature() {
        let fx = Fixture::new();
        let body = b"not a bundle";
        let h = signed_headers(PROJECT, now(), SECRET, body);
        assert!(fx.reject(&h, body).await.contains("decompressing"));
    }

    #[tokio::test]
    async fn rejects_manifest_project_mismatch() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec { project: "other", ..Default::default() });
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        assert!(fx.reject(&h, &body).await.contains("manifest project mismatch"));
    }

    #[tokio::test]
    async fn rejects_unit_not_on_allow_list() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec { units: &["sshd.service"], ..Default::default() });
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        let err = fx.reject(&h, &body).await;
        assert!(err.contains("manifest unit sshd.service was not declared"), "{err}");
        assert!(!fx.install_dir.join("releases").exists());
    }

    #[tokio::test]
    async fn rejects_checksum_mismatch() {
        let fx = Fixture::new();
        let (body, _) = fx.bundle(Spec { corrupt_binary_sha: true, ..Default::default() });
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        assert!(fx.reject(&h, &body).await.contains("checksum mismatch"));
        assert!(!fx.install_dir.join("releases").exists());
    }

    #[tokio::test]
    async fn rejects_existing_release_dir() {
        let fx = Fixture::new();
        let (body, manifest) = fx.bundle(Spec::default());
        let existing = fx.layout().new_release_path(manifest.built_at, &manifest.git_sha);
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("marker"), b"keep me").unwrap();
        let h = signed_headers(PROJECT, now(), SECRET, &body);
        assert!(fx.reject(&h, &body).await.contains("already exists"));
        assert_eq!(fs::read(existing.join("marker")).unwrap(), b"keep me");
    }

    #[tokio::test]
    async fn handler_maps_errors_to_400() {
        // Fails on the first header check, long before connect_system().
        let fx = Fixture::new();
        let (status, body) =
            handle_deploy(State(Arc::new(fx.state.clone())), HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.starts_with("deploy failed: missing required header"));
        assert!(body.ends_with('\n'));
    }

    // ---------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------

    #[test]
    fn header_str_happy_and_missing() {
        let mut h = HeaderMap::new();
        h.insert("x-a", HeaderValue::from_static("v"));
        assert_eq!(header_str(&h, "x-a").unwrap(), "v");
        assert!(header_str(&h, "x-b").unwrap_err().to_string().contains("missing required header"));
    }

    #[test]
    fn header_str_rejects_non_utf8() {
        let mut h = HeaderMap::new();
        h.insert("x-a", HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        assert!(header_str(&h, "x-a").unwrap_err().to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn move_dir_moves_tree_and_creates_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        fs::create_dir_all(from.join("bin")).unwrap();
        fs::write(from.join("bin/app"), b"x").unwrap();
        let to = tmp.path().join("deep/parent/to");
        move_dir(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(to.join("bin/app")).unwrap(), b"x");
    }

    #[test]
    fn move_dir_missing_source_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(move_dir(&tmp.path().join("nope"), &tmp.path().join("to")).is_err());
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        fs::create_dir_all(from.join("a/b")).unwrap();
        fs::write(from.join("top"), b"1").unwrap();
        fs::write(from.join("a/b/leaf"), b"2").unwrap();
        let to = tmp.path().join("to");
        copy_dir_recursive(&from, &to).unwrap();
        assert_eq!(fs::read(to.join("top")).unwrap(), b"1");
        assert_eq!(fs::read(to.join("a/b/leaf")).unwrap(), b"2");
        assert!(from.join("a/b/leaf").exists(), "copy must not remove the source");
    }

    #[test]
    fn copy_dir_recursive_missing_source_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(copy_dir_recursive(&tmp.path().join("nope"), &tmp.path().join("to")).is_err());
    }

    #[test]
    fn rollback_restores_previous_release() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let old = layout.releases_dir().join("old");
        let new = layout.releases_dir().join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        layout.swap_current(&new).unwrap();
        rollback(&layout, Some(&old)).unwrap();
        assert_eq!(layout.current_target().unwrap(), Some(old));
    }

    #[test]
    fn rollback_without_previous_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let new = layout.releases_dir().join("new");
        fs::create_dir_all(&new).unwrap();
        layout.swap_current(&new).unwrap();
        rollback(&layout, None).unwrap();
        assert_eq!(layout.current_target().unwrap(), Some(new));
    }

    #[test]
    fn startup_self_prune_respects_retain_count() {
        // Assumes DEPLOY_AGENT_PROJECT_NAME is unset (default "deploy-agent").
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path().join("etc");
        let install_dir = tmp.path().join("opt");
        fs::create_dir_all(config_root.join("projects")).unwrap();
        let cfg = ProjectConfig {
            install_dir: install_dir.clone(),
            allowed_units: vec![],
            retain_count: 1,
            health_check: None,
            secret_path: "/unused".into(),
        };
        fs::write(config_root.join("projects/deploy-agent.toml"), toml::to_string(&cfg).unwrap())
            .unwrap();
        let layout = ReleaseLayout::new(&install_dir);
        for r in ["r1", "r2", "r3"] {
            fs::create_dir_all(layout.releases_dir().join(r)).unwrap();
        }
        layout.swap_current(&layout.releases_dir().join("r3")).unwrap();

        prune_self_on_startup(&config_root);

        assert_eq!(layout.list_releases().unwrap(), vec![layout.releases_dir().join("r3")]);
    }

    #[test]
    fn startup_self_prune_without_config_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        prune_self_on_startup(tmp.path()); // must not panic
    }
}
