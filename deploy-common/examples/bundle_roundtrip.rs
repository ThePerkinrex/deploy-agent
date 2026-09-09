use deploy_common::{
    bundle,
    manifest::{BinaryEntry, CURRENT_SCHEMA_VERSION, Manifest, UnitEntry},
    release::ReleaseLayout,
};
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

fn main() -> anyhow::Result<()> {
    // 1. Fabricate a fake build output dir
    let src = tempdir()?;
    fs::create_dir_all(src.path().join("bin"))?;
    fs::create_dir_all(src.path().join("systemd"))?;
    fs::create_dir_all(src.path().join("config"))?;
    fs::write(src.path().join("bin/myproj-api"), b"pretend binary bytes")?;
    fs::write(
        src.path().join("systemd/myproj-api.service"),
        "[Unit]\nDescription=fake\n",
    )?;

    let bin_sha = sha256_file(&src.path().join("bin/myproj-api"))?;
    let unit_sha = sha256_file(&src.path().join("systemd/myproj-api.service"))?;

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

    // 2. Build bundle
    let bundle_path = src.path().join("../bundle.tar.zst");
    bundle::build_bundle(src.path(), &bundle_path)?;
    println!(
        "bundle written: {} ({} bytes)",
        bundle_path.display(),
        fs::metadata(&bundle_path)?.len()
    );

    // 3. Extract into a release layout and verify
    let app_root = tempdir()?;
    fs::create_dir_all(app_root.path().join("releases"))?;
    let layout = ReleaseLayout::new(app_root.path());
    let release_dir = layout.new_release_path(manifest.built_at, &manifest.git_sha);
    bundle::extract_bundle(&bundle_path, &release_dir)?;

    let read_back = bundle::read_manifest(&release_dir)?;
    bundle::verify_checksums(&release_dir, &read_back)?;
    layout.swap_current(&release_dir)?;

    println!("current -> {:?}", layout.current_target()?);
    println!("OK");
    Ok(())
}

fn sha256_file(p: &std::path::Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let data = fs::read(p)?;
    Ok(hex::encode(Sha256::digest(&data)))
}
