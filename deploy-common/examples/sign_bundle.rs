use deploy_common::{
    bundle,
    hmac::compute_signature,
    manifest::{BinaryEntry, CURRENT_SCHEMA_VERSION, Manifest, UnitEntry},
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
    fs::write(
        src.path().join("bin/myproj-api"),
        b"pretend binary bytes v1",
    )?;
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
        binaries: vec![BinaryEntry {
            path: "bin/myproj-api".into(),
            sha256: bin_sha,
            mode: 0o755,
        }],
        units: vec![UnitEntry {
            name: "myproj-api.service".into(),
            path: "systemd/myproj-api.service".into(),
            sha256: unit_sha,
        }],
    };
    fs::write(
        src.path().join("manifest.toml"),
        toml::to_string_pretty(&manifest)?,
    )?;

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
