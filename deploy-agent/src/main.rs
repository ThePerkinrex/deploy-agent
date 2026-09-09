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

    let state = Arc::new(AppState { config_root });

    let app = Router::new()
        .route("/deploy", post(handle_deploy))
        // Bundles are compiled binaries + assets — default 2MB axum limit
        // is far too small. 200MB is a generous placeholder; revisited
        // properly in the hardening pass (Step 10).
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(state);

    // Plain HTTP for this step, loopback only. Section 2.6 below swaps
    // this for axum_server::bind_rustls once the core logic is proven.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8443));
    tracing::info!("listening on {addr} (plain HTTP, dev only)");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    match do_deploy(&state, &headers, &body).await {
        Ok(release_id) => (
            StatusCode::OK,
            format!("deployed release {release_id}\n"),
        ),
        Err(e) => {
            tracing::warn!("deploy failed: {e:#}");
            (StatusCode::BAD_REQUEST, format!("deploy failed: {e}\n"))
        }
    }
}

async fn do_deploy(state: &AppState, headers: &HeaderMap, body: &[u8]) -> anyhow::Result<String> {
    // --- 1. Pull and validate headers ---
    let project = header_str(headers, "x-deploy-project")?;
    let timestamp: i64 = header_str(headers, "x-deploy-timestamp")?
        .parse()
        .map_err(|_| anyhow::anyhow!("X-Deploy-Timestamp is not a valid integer"))?;
    let signature_header = header_str(headers, "x-deploy-signature")?;
    let signature_hex = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow::anyhow!("X-Deploy-Signature missing 'sha256=' prefix"))?;

    // --- 2. Replay window ---
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64;
    if (now - timestamp).abs() > REPLAY_WINDOW_SECS {
        anyhow::bail!(
            "timestamp outside replay window: now={now} given={timestamp} (max skew {REPLAY_WINDOW_SECS}s)"
        );
    }

    // --- 3. Load project policy + secret (server-side source of truth —
    //         never trust anything about allowed units/paths from the
    //         bundle itself, only the project name from the header, which
    //         is itself authenticated by the signature that follows) ---
    let project_config_path = state
        .config_root
        .join("projects")
        .join(format!("{project}.toml"));
    let project_config = ProjectConfig::load(&project_config_path)
        .map_err(|e| anyhow::anyhow!("unknown or unreadable project '{project}': {e}"))?;
    let secret_raw = std::fs::read(&project_config.secret_path)
        .map_err(|e| anyhow::anyhow!("reading secret for '{project}': {e}"))?;

    let secret = secret_raw.trim_ascii();

    // --- 4. Verify signature (constant-time compare inside verify_signature) ---
    dhmac::verify_signature(secret, timestamp, project, body, signature_hex)?;
    tracing::info!("signature OK for project '{project}'");

    // --- 5. Extract to a temp staging dir, then move into releases/ once
    //         we know it's good. Extracting straight into releases/<name>/
    //         and leaving a half-unpacked directory there on failure is
    //         exactly the kind of thing the atomic-swap design is meant
    //         to avoid, so stage outside the release tree first. ---
    let staging_dir = tempfile::tempdir()
        .map_err(|e| anyhow::anyhow!("creating staging dir: {e}"))?;
    let bundle_path = staging_dir.path().join("bundle.tar.zst");
    std::fs::write(&bundle_path, body)?;

    let extract_dir = staging_dir.path().join("extracted");
    bundle::extract_bundle(&bundle_path, &extract_dir)?;

    let manifest = bundle::read_manifest(&extract_dir)?;
    if manifest.project != project {
        anyhow::bail!(
            "manifest project '{}' does not match X-Deploy-Project '{project}'",
            manifest.project
        );
    }
    bundle::verify_checksums(&extract_dir, &manifest)?;
    tracing::info!("checksums OK, {} binaries, {} units", manifest.binaries.len(), manifest.units.len());

    // --- 6. Move staged, verified extraction into releases/<name>/, then
    //         atomically swap current -> it ---
    let layout = ReleaseLayout::new(&project_config.install_dir);
    std::fs::create_dir_all(layout.releases_dir())?;
    let release_path = layout.new_release_path(manifest.built_at, &manifest.git_sha);
    if release_path.exists() {
        anyhow::bail!("release dir {} already exists (duplicate deploy?)", release_path.display());
    }
    move_dir(&extract_dir, &release_path)?;
    layout.swap_current(&release_path)?;

    // --- 7. Systemd restart is Step 3. For now, just log intent so you can
    //         see this working end-to-end before wiring D-Bus. ---
    for unit in &manifest.units {
        if project_config.allowed_units.contains(&unit.name) {
            tracing::info!("(stub) would restart allow-listed unit: {}", unit.name);
        } else {
            tracing::warn!(
                "(stub) unit '{}' in manifest is NOT on this project's allow-list, would be refused",
                unit.name
            );
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

/// std::fs::rename fails across filesystems/mount points; fall back to
/// copy+remove if that happens. Cheap insurance for when /tmp and
/// /srv/apps end up on different filesystems on the real Pi.
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

