use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use deploy_common::{
    bundle::build_bundle,
    hmac::compute_signature,
    manifest::{BinaryEntry, Manifest, UnitEntry, CURRENT_SCHEMA_VERSION},
};
use sha2::{Digest, Sha256};
use ureq::{
    config::Config,
    tls::{parse_pem, PemItem, RootCerts, TlsConfig, TlsProvider},
    Agent,
};

#[derive(Debug, Parser)]
#[command(name = "deploy-ci", about = "CI-side deploy bundle builder/sender")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build manifest.toml from a staging dir and tar.zst it into a bundle.
    Bundle {
        /// Directory containing bin/, systemd/, config/ (manifest.toml is written here)
        #[arg(long)]
        staging_dir: PathBuf,
        /// Project name — must match the server-side project config and
        /// the X-Deploy-Project header sent at `send` time.
        #[arg(long)]
        project: String,
        /// Full git commit sha this bundle was built from
        #[arg(long)]
        git_sha: String,
        /// Where to write the resulting .tar.zst bundle
        #[arg(long)]
        out: PathBuf,
        /// Optional build metadata as key=value, repeatable
        /// (e.g. --meta branch=main --meta run_id=12345)
        #[arg(long = "meta", value_parser = parse_key_val)]
        build_meta: Vec<(String, String)>,
    },
    /// Sign and POST a bundle to a running deploy-agent.
    Send {
        /// Path to the .tar.zst bundle produced by `bundle`
        #[arg(long)]
        bundle: PathBuf,
        /// Project name — sent as X-Deploy-Project, must match the
        /// bundle's manifest.project (the agent rejects a mismatch).
        #[arg(long)]
        project: String,
        /// Full URL of the agent's deploy endpoint,
        /// e.g. https://rpi.<tailnet>.ts.net:8443/deploy
        #[arg(long)]
        url: String,
        /// Name of the env var holding the raw HMAC secret
        #[arg(long, default_value = "DEPLOY_HMAC_KEY")]
        hmac_key_env: String,
        /// Path to a PEM file containing the local CA that signed the
        /// agent's self-signed cert. Opt-in: omit this to use the normal
        /// system/webpki root store (which will fail against a self-signed
        /// cert, as expected).
        #[arg(long)]
        ca_cert: Option<PathBuf>,
    },
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s:?}"))?;
    Ok((k.to_string(), v.to_string()))
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Bundle {
            staging_dir,
            project,
            git_sha,
            out,
            build_meta,
        } => cmd_bundle(&staging_dir, &project, &git_sha, &out, build_meta),
        Command::Send {
            bundle,
            project,
            url,
            hmac_key_env,
            ca_cert,
        } => cmd_send(&bundle, &project, &url, &hmac_key_env, ca_cert.as_deref()),
    }
}

fn cmd_bundle(
    staging_dir: &Path,
    project: &str,
    git_sha: &str,
    out: &Path,
    build_meta: Vec<(String, String)>,
) -> Result<()> {
    let binaries: Vec<BinaryEntry> = hash_entries(&staging_dir.join("bin"), "bin")
        .context("hashing bin/ contents")?
        .into_iter()
        .map(|(path, sha256, mode)| BinaryEntry { path, sha256, mode })
        .collect();

    if binaries.is_empty() {
        bail!("no files found under {}/bin", staging_dir.display());
    }

    let units: Vec<UnitEntry> = hash_entries(&staging_dir.join("systemd"), "systemd")
        .context("hashing systemd/ contents")?
        .into_iter()
        .map(|(path, sha256, _mode)| UnitEntry {
            name: Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path)
                .to_string(),
            path,
            sha256,
        })
        .collect();

    let manifest = Manifest {
        schema_version: CURRENT_SCHEMA_VERSION,
        project: project.to_string(),
        git_sha: git_sha.to_string(),
        built_at: time::OffsetDateTime::now_utc(),
        build_meta: build_meta.into_iter().collect::<BTreeMap<_, _>>(),
        binaries,
        units,
    };

    let manifest_toml = toml::to_string_pretty(&manifest).context("serializing manifest.toml")?;
    fs::write(staging_dir.join("manifest.toml"), manifest_toml)
        .context("writing manifest.toml into staging dir")?;

    build_bundle(staging_dir, out)
        .with_context(|| format!("building bundle at {}", out.display()))?;

    println!("wrote bundle: {}", out.display());
    Ok(())
}

/// Reads one directory level (bin/ and systemd/ are flat per the bundle
/// layout) and returns (path-relative-to-bundle-root, hex sha256, unix mode)
/// for each regular file in it. Missing directory -> empty, not an error,
/// since e.g. a project with no unit changes may ship an empty systemd/.
fn hash_entries(dir: &Path, root_prefix: &str) -> Result<Vec<(String, String, u32)>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let mode = file_mode(&path)?;
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-utf8 file name in {}", dir.display()))?;
        out.push((format!("{root_prefix}/{file_name}"), sha256, mode));
    }
    Ok(out)
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<u32> {
    Ok(0o755)
}

fn cmd_send(
    bundle_path: &Path,
    project: &str,
    url: &str,
    hmac_key_env: &str,
    ca_cert: Option<&Path>,
) -> Result<()> {
    let secret = std::env::var(hmac_key_env)
        .with_context(|| format!("reading HMAC secret from env var {hmac_key_env}"))?;
    let body = fs::read(bundle_path)
        .with_context(|| format!("reading bundle {}", bundle_path.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs() as i64;

    let signature = compute_signature(secret.as_bytes(), timestamp, project, &body)
        .context("computing HMAC signature")?;

    let agent = build_agent(ca_cert)?;

    let mut response = agent
        .post(url)
        .header("X-Deploy-Project", project)
        .header("X-Deploy-Timestamp", timestamp.to_string())
        .header("X-Deploy-Signature", format!("sha256={signature}"))
        .content_type("application/octet-stream")
        .send(&body[..])
        .with_context(|| format!("sending bundle to {url}"))?;

    let status = response.status();
    if status.is_success() {
        println!("deploy accepted: {status}");
        return Ok(());
    }

    let body_text = response
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|_| "<failed to read response body>".to_string());
    bail!("deploy rejected: HTTP {status}: {body_text}");
}

/// Builds the ureq Agent used for `send`. With `ca_cert: None`, this is the
/// normal webpki/system root store — talking to the agent's self-signed
/// cert will fail TLS verification, which is the correct default behavior.
/// Passing `ca_cert` opts in to trusting exactly that one local CA (and
/// nothing else) instead of disabling verification entirely.
fn build_agent(ca_cert: Option<&Path>) -> Result<Agent> {
    let mut tls_builder = TlsConfig::builder().provider(TlsProvider::Rustls);

    if let Some(path) = ca_cert {
        let pem_data =
            fs::read(path).with_context(|| format!("reading CA cert {}", path.display()))?;
        let certs: Vec<_> = parse_pem(&pem_data)
            .filter_map(|item| match item {
                Ok(PemItem::Certificate(cert)) => Some(cert),
                _ => None,
            })
            .collect();
        if certs.is_empty() {
            bail!("no certificates found in {} (expected PEM)", path.display());
        }
        tls_builder = tls_builder.root_certs(RootCerts::new_with_certs(&certs));
    }

    // Read status codes as ordinary responses rather than errors, so a
    // non-2xx reply still gets its body read for the failure message below
    // instead of being swallowed into a bare Error::StatusCode(code).
    let config = Config::builder()
        .tls_config(tls_builder.build())
        .http_status_as_error(false)
        .build();

    Ok(config.new_agent())
}
