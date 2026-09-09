use crate::manifest::Manifest;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

/// Writes a tar.zst bundle from a source directory containing
/// manifest.toml, bin/, systemd/, config/ at its root.
pub fn build_bundle(src_dir: &Path, out_path: &Path) -> Result<()> {
    let tar_data = {
        let mut buf = Vec::new();
        let mut builder = tar::Builder::new(&mut buf);
        builder.append_dir_all(".", src_dir)?;
        builder.finish()?;
        drop(builder);
        buf
    };
    let compressed = zstd::stream::encode_all(tar_data.as_slice(), 19)?;
    fs::write(out_path, compressed)?;
    Ok(())
}

/// Extracts a tar.zst bundle into `dest_dir`, refusing anything that looks
/// like a path-traversal or symlink-escape attempt. Treat the archive as
/// hostile input even though it arrived over an HMAC-verified channel —
/// the signature proves *who sent it*, not that its contents are safe to
/// blindly extract.
pub fn extract_bundle(bundle_path: &Path, dest_dir: &Path) -> Result<()> {
    let compressed = fs::read(bundle_path)?;
    let tar_data = zstd::stream::decode_all(compressed.as_slice())?;
    let mut archive = tar::Archive::new(tar_data.as_slice());

    fs::create_dir_all(dest_dir)?;
    let dest_canon = dest_dir.canonicalize()?;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // The archive root itself (written by append_dir_all(".", src_dir))
        // shows up as a "." entry. dest_dir already exists — nothing to do.
        if path == Path::new(".") {
            continue;
        }

        // Reject absolute paths and any ".." component outright.
        for comp in path.components() {
            match comp {
                Component::Normal(_) => {}
                Component::CurDir => {}
                _ => bail!("bundle entry has unsafe path component: {:?}", path),
            }
        }

        // Reject symlinks entirely — this bundle format has no legitimate
        // use for them, and they're the classic tar-slip vector.
        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            bail!("bundle entry is a link, refusing: {:?}", path);
        }

        let target = dest_dir.join(&path);
        // Belt-and-suspenders: after joining, confirm the resolved parent
        // is still inside dest_dir.
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
            let parent_canon = parent
                .canonicalize()
                .context("resolving extracted entry parent")?;
            if !parent_canon.starts_with(&dest_canon) {
                bail!("bundle entry escapes destination dir: {:?}", path);
            }
        }

        entry.unpack(&target)?;
    }

    Ok(())
}

/// Reads manifest.toml out of an already-extracted release directory.
pub fn read_manifest(release_dir: &Path) -> Result<Manifest> {
    let manifest_path = release_dir.join("manifest.toml");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_str(&raw)?;
    if manifest.schema_version != crate::manifest::CURRENT_SCHEMA_VERSION {
        bail!(
            "unsupported manifest schema_version {} (expected {})",
            manifest.schema_version,
            crate::manifest::CURRENT_SCHEMA_VERSION
        );
    }
    Ok(manifest)
}

/// Verifies every binary and unit file listed in the manifest against its
/// recorded sha256. Call this AFTER extraction, BEFORE the symlink swap.
pub fn verify_checksums(release_dir: &Path, manifest: &Manifest) -> Result<()> {
    for bin in &manifest.binaries {
        verify_one(&release_dir.join(&bin.path), &bin.sha256)?;
    }
    for unit in &manifest.units {
        verify_one(&release_dir.join(&unit.path), &unit.sha256)?;
    }
    Ok(())
}

fn verify_one(path: &Path, expected_hex: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut f =
        fs::File::open(path).with_context(|| format!("opening {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    if got != expected_hex.to_lowercase() {
        bail!(
            "checksum mismatch for {}: expected {}, got {}",
            path.display(),
            expected_hex,
            got
        );
    }
    Ok(())
}
