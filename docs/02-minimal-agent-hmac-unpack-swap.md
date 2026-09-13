# Step 2: Minimal Agent — HTTP Endpoint, HMAC Verify, Unpack → Stage → Swap

Scope for this step, per the build order: a single endpoint, HMAC verification,
unpack → stage → symlink swap. **No systemd yet** — after swapping `current`,
the handler just logs which units it *would* restart. D-Bus/systemd wiring is
Step 3.

We also add TLS at the end of this guide, since it's a confirmed decision
(agent terminates TLS itself even inside the tailnet) — but it's kept separate
from the core logic so you can prove HMAC + unpack + swap work over plain
HTTP on loopback first, without a cert-trust question muddying the picture.

---

## 2.1 — What this step adds to the workspace

```
deploy-agent-workspace/
  deploy-common/
    src/
      manifest.rs   (from Step 1)
      bundle.rs      (from Step 1, with the "." extraction fix applied)
      release.rs     (from Step 1)
      hmac.rs         <- NEW: canonical signing-string + verify, shared by
                            deploy-agent now and deploy-ci later
      project.rs      <- NEW: ProjectConfig, the server-side policy struct
      lib.rs          (re-export the above)
  deploy-agent/
    Cargo.toml
    src/
      main.rs         <- NEW: axum server, /deploy handler
```

Add to `deploy-common/Cargo.toml`:

```toml
[dependencies]
# ...existing deps from Step 1...
hmac = "0.12"
```

(`sha2` and `hex` are already there from Step 1.)

---

## 2.2 — Canonical HMAC signing string (`deploy-common/src/hmac.rs`)

This is the one place the exact signed byte sequence is constructed. Both
`deploy-agent` (verifier, this step) and `deploy-ci` (signer, Step 7) call
into this — nothing about signature construction gets reimplemented on
either side.

```rust
use anyhow::{bail, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Builds the exact string that gets HMAC-signed:
///   "<unix_timestamp>\n<project>\n<hex sha256 of body>"
/// Constructing this in one place is the whole point — get this wrong on
/// either the signing or verifying side and every deploy fails with an
/// opaque "signature mismatch," so it's tested directly (see tests below)
/// rather than only exercised indirectly through the HTTP layer.
pub fn signing_string(timestamp: i64, project: &str, body: &[u8]) -> String {
    let body_hash = hex::encode(Sha256::digest(body));
    format!("{timestamp}\n{project}\n{body_hash}")
}

/// Computes the hex-encoded HMAC-SHA256 tag for a signing string, given the
/// project's secret. Used by deploy-ci to produce the X-Deploy-Signature
/// header value (as "sha256=<hex>").
pub fn compute_signature(secret: &[u8], timestamp: i64, project: &str, body: &[u8]) -> Result<String> {
    let msg = signing_string(timestamp, project, body);
    let mut mac = HmacSha256::new_from_slice(secret)?;
    mac.update(msg.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verifies a provided signature (hex, WITHOUT the "sha256=" prefix — strip
/// that in the caller) against the secret, timestamp, project, and body.
/// Uses `Mac::verify_slice`, which does a constant-time comparison
/// internally — do not replace this with a manual `==` on hex strings or
/// byte arrays, which would reintroduce a timing side channel.
pub fn verify_signature(
    secret: &[u8],
    timestamp: i64,
    project: &str,
    body: &[u8],
    provided_signature_hex: &str,
) -> Result<()> {
    let msg = signing_string(timestamp, project, body);
    let mut mac = HmacSha256::new_from_slice(secret)?;
    mac.update(msg.as_bytes());

    let provided_bytes = match hex::decode(provided_signature_hex) {
        Ok(b) => b,
        Err(_) => bail!("signature is not valid hex"),
    };

    mac.verify_slice(&provided_bytes)
        .map_err(|_| anyhow::anyhow!("signature verification failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let secret = b"test-secret-do-not-use-in-prod";
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", body).unwrap();
        assert!(verify_signature(secret, ts, "myproj", body, &sig).is_ok());
    }

    #[test]
    fn rejects_wrong_secret() {
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(b"secret-a", ts, "myproj", body).unwrap();
        assert!(verify_signature(b"secret-b", ts, "myproj", body, &sig).is_err());
    }

    #[test]
    fn rejects_tampered_body() {
        let secret = b"test-secret-do-not-use-in-prod";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", b"original").unwrap();
        assert!(verify_signature(secret, ts, "myproj", b"tampered!", &sig).is_err());
    }

    #[test]
    fn rejects_wrong_project() {
        let secret = b"test-secret-do-not-use-in-prod";
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", body).unwrap();
        assert!(verify_signature(secret, ts, "otherproj", body, &sig).is_err());
    }
}
```

Run `cargo test -p deploy-common hmac::` — all four should pass before moving on.

---

## 2.3 — Project policy (`deploy-common/src/project.rs`)

The handoff specifies the shape of `/etc/deploy-agent/projects/<name>.toml`.
Defining the struct in `deploy-common` now means `deploy-admin` (Step 9) can
write it and `deploy-agent` can read it without a second implementation.
`allowed_units` and `health_check` aren't *used* yet in this step (no systemd,
no health checks until Steps 3/6) but the struct should be complete now so
the on-disk format doesn't change out from under you later.

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// e.g. /srv/apps/myproj  (or /opt/deploy-agent for the self project)
    pub install_dir: PathBuf,

    /// Systemd unit names this agent may touch for this project. Enforced
    /// again at the OS level by the polkit rule (Step 4) — this is a
    /// second, defense-in-depth check at the application layer.
    #[serde(default)]
    pub allowed_units: Vec<String>,

    #[serde(default = "default_retain_count")]
    pub retain_count: usize,

    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,

    /// Path to this project's HMAC secret file, e.g.
    /// /etc/deploy-agent/secrets/myproj.key
    pub secret_path: PathBuf,
}

fn default_retain_count() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub url: String,
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
}

fn default_health_timeout() -> u64 {
    10
}

impl ProjectConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading project config {}: {e}", path.display()))?;
        Ok(toml::from_str(&raw)?)
    }
}
```

`deploy-common/src/lib.rs` should now look like:

```rust
pub mod bundle;
pub mod hmac;
pub mod manifest;
pub mod project;
pub mod release;
```

---

## 2.4 — The agent itself (`deploy-agent`)

`deploy-agent/Cargo.toml`:

```toml
[package]
name = "deploy-agent"
version = "0.1.0"
edition = "2021"

[dependencies]
deploy-common = { path = "../deploy-common" }
axum = "0.7"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
hex = "0.4"
time = { version = "0.3", features = ["formatting"] }
```

`deploy-agent/src/main.rs`:

```rust
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
    let secret = std::fs::read(&project_config.secret_path)
        .map_err(|e| anyhow::anyhow!("reading secret for '{project}': {e}"))?;

    // --- 4. Verify signature (constant-time compare inside verify_signature) ---
    dhmac::verify_signature(&secret, timestamp, project, body, signature_hex)?;
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
```

Add `tempfile = "3"` to `deploy-agent/Cargo.toml` dependencies (dev-only in
`deploy-common`, but a real runtime dependency here since the agent uses it
for staging on every deploy).

Note the project-name cross-check in step 5 (`manifest.project != project`):
the header's project name gates *which secret verifies the signature*, but
nothing stops someone who legitimately holds `myproj`'s secret from crafting
a bundle whose internal manifest claims to be a different project. Checking
both keeps the header and the manifest honest about each other rather than
trusting either alone.

---

## 2.5 — Manual test, no TLS, no systemd

### Set up a scratch config root

```bash
mkdir -p /tmp/deploy-agent-test/{projects,secrets}
mkdir -p /tmp/deploy-agent-test/apps/myproj
head -c 32 /dev/urandom | xxd -p -c 256 > /tmp/deploy-agent-test/secrets/myproj.key

cat > /tmp/deploy-agent-test/projects/myproj.toml <<'EOF'
install_dir = "/tmp/deploy-agent-test/apps/myproj"
allowed_units = ["myproj-api.service"]
retain_count = 5
secret_path = "/tmp/deploy-agent-test/secrets/myproj.key"
EOF
```

### Start the agent

```bash
DEPLOY_AGENT_CONFIG_ROOT=/tmp/deploy-agent-test RUST_LOG=info \
  cargo run -p deploy-agent
```

### Build a small helper to construct + sign a test bundle

Add `deploy-common/examples/sign_bundle.rs` — this reuses the exact same
`hmac::compute_signature` that `deploy-ci` will use in Step 7, so this isn't
throwaway test code, it's an early proof that the shared signing logic
works end to end.

```rust
use deploy_common::{
    bundle,
    hmac::compute_signature,
    manifest::{BinaryEntry, Manifest, UnitEntry, CURRENT_SCHEMA_VERSION},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> anyhow::Result<()> {
    let src = tempfile::tempdir()?;
    fs::create_dir_all(src.path().join("bin"))?;
    fs::create_dir_all(src.path().join("systemd"))?;
    fs::create_dir_all(src.path().join("config"))?;
    fs::write(src.path().join("bin/myproj-api"), b"pretend binary bytes v1")?;
    fs::write(
        src.path().join("systemd/myproj-api.service"),
        "[Unit]\nDescription=fake\n[Service]\nExecStart=/srv/apps/myproj/current/bin/myproj-api\n",
    )?;

    let bin_sha = hex::encode(Sha256::digest(fs::read(src.path().join("bin/myproj-api"))?));
    let unit_sha = hex::encode(Sha256::digest(fs::read(
        src.path().join("systemd/myproj-api.service"),
    )?));

    let manifest = Manifest {
        schema_version: CURRENT_SCHEMA_VERSION,
        project: "myproj".into(),
        git_sha: "abc1234def5678900000000000000000000000".into(),
        built_at: time::OffsetDateTime::now_utc(),
        build_meta: BTreeMap::new(),
        binaries: vec![BinaryEntry { path: "bin/myproj-api".into(), sha256: bin_sha, mode: 0o755 }],
        units: vec![UnitEntry {
            name: "myproj-api.service".into(),
            path: "systemd/myproj-api.service".into(),
            sha256: unit_sha,
        }],
    };
    fs::write(src.path().join("manifest.toml"), toml::to_string_pretty(&manifest)?)?;

    let bundle_path = std::path::Path::new("/tmp/test-bundle.tar.zst");
    bundle::build_bundle(src.path(), bundle_path)?;
    let body = fs::read(bundle_path)?;

    let secret = fs::read("/tmp/deploy-agent-test/secrets/myproj.key")?;
    let secret = secret.trim_ascii(); // strip trailing newline from xxd/echo
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let signature = compute_signature(secret, timestamp, "myproj", &body)?;

    println!("BUNDLE_PATH={}", bundle_path.display());
    println!("TIMESTAMP={timestamp}");
    println!("SIGNATURE=sha256={signature}");
    Ok(())
}
```

```bash
cargo run --example sign_bundle -p deploy-common
```

This prints something like:

```
BUNDLE_PATH=/tmp/test-bundle.tar.zst
TIMESTAMP=1757430000
SIGNATURE=sha256=3f9a2c...
```

### Send it

```bash
curl -i -X POST http://127.0.0.1:8443/deploy \
  -H "X-Deploy-Project: myproj" \
  -H "X-Deploy-Timestamp: 1757430000" \
  -H "X-Deploy-Signature: sha256=3f9a2c..." \
  --data-binary @/tmp/test-bundle.tar.zst
```

(substitute the real timestamp/signature values from the previous step)

Expected: `200 OK`, body like `deployed release 2026-...abc1234def5.`, and the
agent's logs show `signature OK`, checksum counts, and the `(stub) would
restart allow-listed unit: myproj-api.service` line.

Verify on disk:

```bash
ls -la /tmp/deploy-agent-test/apps/myproj/
readlink /tmp/deploy-agent-test/apps/myproj/current
cat /tmp/deploy-agent-test/apps/myproj/current/manifest.toml
```

### Negative-path checks worth doing by hand

- Re-send the exact same curl command a few minutes later with the same
  (now-stale) timestamp → should get `400` with a replay-window message.
- Flip one character in the signature → `400 deploy failed: signature
  verification failed`.
- Change `X-Deploy-Project` to a project with no `projects/<name>.toml` →
  `400 unknown or unreadable project`.
- Corrupt one byte in `/tmp/test-bundle.tar.zst` with `dd` before sending →
  either extraction or checksum verification should fail, not silently
  succeed.

---

## 2.6 — Add TLS

Now that the core logic is proven, wire in `axum-server` + `rustls`, per the
handoff's decision to terminate TLS in-agent even inside the tailnet.

Add to `deploy-agent/Cargo.toml`:

```toml
axum-server = { version = "0.7", features = ["tls-rustls"] }
```

Generate a local self-signed cert for testing (production cert-provisioning
process — real local CA — is a separate concern to finalize when the agent
actually binds to the Tailscale interface, not needed for this test):

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout /tmp/deploy-agent-test/key.pem \
  -out /tmp/deploy-agent-test/cert.pem \
  -subj "/CN=localhost"
```

Change the end of `main()`:

```rust
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

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8443));
    tracing::info!("listening on {addr} (TLS)");
    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service())
        .await
        .unwrap();
```

Remove the now-unused `tokio::net::TcpListener` / `axum::serve` lines and
the `use axum::routing::post` stays; you no longer need
`tokio::net::TcpListener` directly.

Re-test with `curl -k` (skip cert verification — fine for this local
self-signed loopback test; `deploy-ci` in Step 7/8 will do proper pinned-CA
trust rather than blind trust-all, since it's talking to a real Pi over the
tailnet, not localhost):

```bash
curl -ik -X POST https://127.0.0.1:8443/deploy \
  -H "X-Deploy-Project: myproj" \
  -H "X-Deploy-Timestamp: $(date +%s)" \
  -H "X-Deploy-Signature: sha256=..." \
  --data-binary @/tmp/test-bundle.tar.zst
```

(you'll need to re-run `sign_bundle` to get a fresh, non-stale timestamp/signature)

---

## 2.7 — Exit criteria for this step

- [ ] `cargo test -p deploy-common` passes, including the four new `hmac::` tests
- [ ] Agent starts against a scratch `DEPLOY_AGENT_CONFIG_ROOT`, no real `/etc` paths touched
- [ ] A signed bundle built via `sign_bundle` deploys successfully: `200 OK`, `current` symlink points at the new release, `manifest.toml` inside it matches what was sent
- [ ] Replay-window rejection, bad-signature rejection, unknown-project rejection, and corrupted-bundle rejection all produce `400` with a sensible message, not a panic or `500`
- [ ] Log output shows the `(stub) would restart allow-listed unit` line for `myproj-api.service`, and would show the "NOT on allow-list" warning if you edit `projects/myproj.toml` to remove it from `allowed_units` and redeploy
- [ ] TLS variant works with `curl -k` once cert/key are generated

Once these hold, move to **Step 3: D-Bus integration via `zbus`** — connect
to `org.freedesktop.systemd1`, implement `StartUnit`/`StopUnit`/`RestartUnit`/
`Reload`, and subscribe to `JobRemoved` so the stub log lines above become
real restarts. Want that guide next?
