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
    send_signed(bundle_path, project, url, secret.as_bytes(), ca_cert)
}

/// Signs `bundle_path` with `secret` and POSTs it to `url`. Split out of
/// `cmd_send` so it can be tested without touching process env vars.
fn send_signed(
    bundle_path: &Path,
    project: &str,
    url: &str,
    secret: &[u8],
    ca_cert: Option<&Path>,
) -> Result<()> {
    let body = fs::read(bundle_path)
        .with_context(|| format!("reading bundle {}", bundle_path.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs() as i64;

    let signature = compute_signature(secret, timestamp, project, &body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use deploy_common::{bundle, hmac::verify_signature};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use tempfile::tempdir;

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn write_staging(dir: &Path) {
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("systemd")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::write(dir.join("bin/api"), b"api-bin").unwrap();
        fs::set_permissions(dir.join("bin/api"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("bin/worker"), b"worker-bin").unwrap();
        fs::set_permissions(dir.join("bin/worker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.join("systemd/api.service"), b"[Service]\n").unwrap();
        fs::write(dir.join("config/app.toml"), b"x = 1\n").unwrap();
    }

    // ---- parse_key_val ----

    #[test]
    fn parse_key_val_splits_on_first_equals() {
        assert_eq!(parse_key_val("branch=main").unwrap(), ("branch".into(), "main".into()));
        assert_eq!(parse_key_val("k=a=b").unwrap(), ("k".into(), "a=b".into()));
        assert_eq!(parse_key_val("k=").unwrap(), ("k".into(), "".into()));
    }

    #[test]
    fn parse_key_val_requires_equals() {
        assert!(parse_key_val("novalue").unwrap_err().contains("expected key=value"));
    }

    // ---- hash_entries ----

    #[test]
    fn hash_entries_missing_dir_is_empty() {
        let tmp = tempdir().unwrap();
        assert!(hash_entries(&tmp.path().join("nope"), "bin").unwrap().is_empty());
    }

    #[test]
    fn hash_entries_hashes_flat_files_with_mode() {
        let tmp = tempdir().unwrap();
        write_staging(tmp.path());
        fs::create_dir_all(tmp.path().join("bin/nested")).unwrap();
        fs::write(tmp.path().join("bin/nested/ignored"), b"x").unwrap();

        let mut entries = hash_entries(&tmp.path().join("bin"), "bin").unwrap();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("bin/api".to_string(), sha(b"api-bin"), 0o755),
                ("bin/worker".to_string(), sha(b"worker-bin"), 0o700),
            ]
        );
    }

    // ---- cmd_bundle ----

    #[test]
    fn bundle_writes_manifest_and_archive() {
        let tmp = tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write_staging(&staging);
        let out = tmp.path().join("b.tar.zst");

        cmd_bundle(&staging, "myproj", "abc123", &out, vec![("branch".into(), "main".into())])
            .unwrap();

        let extracted = tmp.path().join("x");
        bundle::extract_bundle(&out, &extracted).unwrap();
        let m = bundle::read_manifest(&extracted).unwrap();
        assert_eq!(m.project, "myproj");
        assert_eq!(m.git_sha, "abc123");
        assert_eq!(m.build_meta["branch"], "main");
        assert_eq!(m.binaries.len(), 2);
        assert_eq!(m.units.len(), 1);
        assert_eq!(m.units[0].name, "api.service");
        assert_eq!(m.units[0].path, "systemd/api.service");
        bundle::verify_checksums(&extracted, &m).unwrap();
        assert!(extracted.join("config/app.toml").is_file());
        // manifest.toml is also left in the staging dir
        assert!(staging.join("manifest.toml").is_file());
    }

    #[test]
    fn bundle_without_systemd_dir_has_no_units() {
        let tmp = tempdir().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(staging.join("bin")).unwrap();
        fs::write(staging.join("bin/app"), b"x").unwrap();
        let out = tmp.path().join("b.tar.zst");
        cmd_bundle(&staging, "p", "sha", &out, vec![]).unwrap();
        let x = tmp.path().join("x");
        bundle::extract_bundle(&out, &x).unwrap();
        assert!(bundle::read_manifest(&x).unwrap().units.is_empty());
    }

    #[test]
    fn bundle_requires_binaries() {
        let tmp = tempdir().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(staging.join("bin")).unwrap();
        let err = cmd_bundle(&staging, "p", "sha", &tmp.path().join("b"), vec![]).unwrap_err();
        assert!(err.to_string().contains("no files found"));
    }

    #[test]
    fn bundle_fails_when_output_dir_missing() {
        let tmp = tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write_staging(&staging);
        let out = tmp.path().join("no/such/dir/b.tar.zst");
        assert!(cmd_bundle(&staging, "p", "sha", &out, vec![]).is_err());
    }

    // ---- build_agent ----

    #[test]
    fn build_agent_without_ca_uses_default_roots() {
        assert!(build_agent(None).is_ok());
    }

    #[test]
    fn build_agent_missing_ca_file_fails() {
        let err = build_agent(Some(Path::new("/nonexistent/ca.pem"))).unwrap_err();
        assert!(err.to_string().contains("reading CA cert"));
    }

    #[test]
    fn build_agent_rejects_file_without_certificates() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("ca.pem");
        fs::write(&p, b"not a pem file").unwrap();
        let err = build_agent(Some(&p)).unwrap_err();
        assert!(err.to_string().contains("no certificates found"));
    }

    // ---- send ----

    struct Captured {
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    /// One-shot HTTP server on 127.0.0.1 that records the request and
    /// replies with `status` / `reply`.
    fn serve_once(status: &'static str, reply: &'static str) -> (String, mpsc::Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/deploy", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = HashMap::new();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // request line
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let l = line.trim_end();
                if l.is_empty() {
                    break;
                }
                let (k, v) = l.split_once(':').unwrap();
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
            let len: usize = headers["content-length"].parse().unwrap();
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            tx.send(Captured { headers, body }).unwrap();
        });
        (url, rx)
    }

    fn sample_bundle(dir: &Path) -> PathBuf {
        let p = dir.join("b.tar.zst");
        fs::write(&p, b"bundle bytes").unwrap();
        p
    }

    #[test]
    fn send_signs_and_posts_bundle() {
        let tmp = tempdir().unwrap();
        let bundle = sample_bundle(tmp.path());
        let (url, rx) = serve_once("200 OK", "deployed release r1\n");

        send_signed(&bundle, "myproj", &url, b"secret", None).unwrap();

        let req = rx.recv().unwrap();
        assert_eq!(req.body, b"bundle bytes");
        assert_eq!(req.headers["x-deploy-project"], "myproj");
        assert_eq!(req.headers["content-type"], "application/octet-stream");
        let ts: i64 = req.headers["x-deploy-timestamp"].parse().unwrap();
        let sig = req.headers["x-deploy-signature"].strip_prefix("sha256=").unwrap();
        verify_signature(b"secret", ts, "myproj", &req.body, sig).unwrap();
    }

    #[test]
    fn send_reports_rejection_with_body() {
        let tmp = tempdir().unwrap();
        let bundle = sample_bundle(tmp.path());
        let (url, _rx) = serve_once("400 Bad Request", "deploy failed: manifest project mismatch\n");

        let err = send_signed(&bundle, "myproj", &url, b"secret", None).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("manifest project mismatch"), "{msg}");
    }

    #[test]
    fn send_fails_on_missing_bundle() {
        let tmp = tempdir().unwrap();
        let err = send_signed(&tmp.path().join("nope"), "p", "http://127.0.0.1:1/", b"s", None)
            .unwrap_err();
        assert!(err.to_string().contains("reading bundle"));
    }

    #[test]
    fn send_fails_when_agent_unreachable() {
        let tmp = tempdir().unwrap();
        let bundle = sample_bundle(tmp.path());
        // Bind then drop to get a port nobody is listening on.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let err = send_signed(&bundle, "p", &format!("http://127.0.0.1:{port}/deploy"), b"s", None)
            .unwrap_err();
        assert!(err.to_string().contains("sending bundle"));
    }

    #[test]
    fn cmd_send_requires_secret_env_var() {
        let tmp = tempdir().unwrap();
        let bundle = sample_bundle(tmp.path());
        let err = cmd_send(&bundle, "p", "http://127.0.0.1:1/", "DEPLOY_CI_TEST_UNSET_VAR_7f3a", None)
            .unwrap_err();
        assert!(err.to_string().contains("DEPLOY_CI_TEST_UNSET_VAR_7f3a"));
    }
}
