use crate::manifest::Manifest;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{self, Read};
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

/// Upper bound on a bundle's decompressed (tar) size. The compressed body
/// is capped by the agent's HTTP body limit, but zstd can expand that by
/// orders of magnitude; without this cap a crafted bundle could fill the
/// disk during extraction.
pub const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Unpacks a bundle file on disk. See [`extract_bundle_from_reader`].
pub fn extract_bundle(bundle_path: &Path, dest_dir: &Path) -> Result<()> {
    let file =
        fs::File::open(bundle_path).with_context(|| format!("reading {}", bundle_path.display()))?;
    extract_bundle_from_reader(file, dest_dir)
}

/// Unpacks the bundle sent
///
/// Streams a tar.zst bundle from `compressed` into `dest_dir` (no
/// intermediate copy of the compressed or decompressed data is held in
/// memory), refusing anything that looks like a path-traversal or
/// symlink-escape attempt, and anything that decompresses past
/// [`MAX_UNPACKED_BYTES`]. Treat the archive as hostile input even though it
/// arrived over an HMAC-verified channel — the signature proves *who sent
/// it*, not that its contents are safe to blindly extract.
pub fn extract_bundle_from_reader(compressed: impl Read, dest_dir: &Path) -> Result<()> {
    extract_limited(compressed, dest_dir, MAX_UNPACKED_BYTES)
}

/// Wraps the zstd decoder: tags its errors as decompression failures and
/// errors out (rather than silently truncating, as `Read::take` would)
/// once more than `limit` bytes have been produced.
struct GuardedReader<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: Read> Read for GuardedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self
            .inner
            .read(buf)
            .map_err(|e| io::Error::new(e.kind(), format!("decompressing bundle: {e}")))?;
        self.remaining = self.remaining.checked_sub(n as u64).ok_or_else(|| {
            io::Error::other(format!(
                "bundle exceeds maximum unpacked size of {} bytes",
                self.limit
            ))
        })?;
        Ok(n)
    }
}

fn extract_limited(compressed: impl Read, dest_dir: &Path, limit: u64) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(compressed).context("decompressing bundle")?;
    let mut archive = tar::Archive::new(GuardedReader {
        inner: decoder,
        remaining: limit,
        limit,
    });

    fs::create_dir_all(dest_dir).with_context(|| format!("creating {}", dest_dir.display()))?;
    let dest_canon = dest_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dest_dir.display()))?;

    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let path = entry.path().context("reading tar entry path")?.into_owned();

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
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            let parent_canon = parent
                .canonicalize()
                .context("resolving extracted entry parent")?;
            if !parent_canon.starts_with(&dest_canon) {
                bail!("bundle entry escapes destination dir: {:?}", path);
            }
        }

        entry
            .unpack(&target)
            .with_context(|| format!("unpacking {} to {}", path.display(), target.display()))?;
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
    validate_manifest_paths(&manifest)?;
    Ok(manifest)
}

/// Every path in the manifest is later joined onto the release dir (to
/// checksum it, and to copy units into the agent's units dir), so each
/// one must be a plain relative path that stays inside the bundle — an
/// absolute path or `..` would make the agent read files from elsewhere
/// on the host. Unit names become file names, so they must be bare names.
fn validate_manifest_paths(manifest: &Manifest) -> Result<()> {
    let paths = manifest
        .binaries
        .iter()
        .map(|b| &b.path)
        .chain(manifest.units.iter().map(|u| &u.path));
    for p in paths {
        let path = Path::new(p);
        if p.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!("manifest path {p:?} must be a relative path inside the bundle");
        }
    }
    for unit in &manifest.units {
        let mut comps = Path::new(&unit.name).components();
        if !matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None))
            || unit.name.contains('/')
        {
            bail!("manifest unit name {:?} must be a bare file name", unit.name);
        }
    }
    Ok(())
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
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {} for checksum", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BinaryEntry, CURRENT_SCHEMA_VERSION, UnitEntry};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn sample_manifest(bin: &[u8], unit: &[u8]) -> Manifest {
        Manifest {
            schema_version: CURRENT_SCHEMA_VERSION,
            project: "myproj".into(),
            git_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            built_at: time::OffsetDateTime::now_utc(),
            build_meta: BTreeMap::new(),
            binaries: vec![BinaryEntry {
                path: "bin/app".into(),
                sha256: sha(bin),
                mode: 0o755,
            }],
            units: vec![UnitEntry {
                name: "app.service".into(),
                path: "systemd/app.service".into(),
                sha256: sha(unit),
            }],
        }
    }

    /// Writes a valid staging dir (bin/, systemd/, config/, manifest.toml).
    fn write_staging(dir: &Path) -> Manifest {
        let bin = b"binary bytes";
        let unit = b"[Service]\nExecStart=/bin/true\n";
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("systemd")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::write(dir.join("bin/app"), bin).unwrap();
        fs::set_permissions(dir.join("bin/app"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("systemd/app.service"), unit).unwrap();
        fs::write(dir.join("config/app.toml"), "key = 1\n").unwrap();
        let manifest = sample_manifest(bin, unit);
        fs::write(
            dir.join("manifest.toml"),
            toml::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    fn build_sample_bundle(root: &Path) -> (std::path::PathBuf, Manifest) {
        let staging = root.join("staging");
        let manifest = write_staging(&staging);
        let out = root.join("bundle.tar.zst");
        build_bundle(&staging, &out).unwrap();
        (out, manifest)
    }

    #[test]
    fn build_extract_verify_round_trip() {
        let tmp = tempdir().unwrap();
        let (bundle, manifest) = build_sample_bundle(tmp.path());
        let dest = tmp.path().join("out");

        extract_bundle(&bundle, &dest).unwrap();

        assert_eq!(fs::read(dest.join("bin/app")).unwrap(), b"binary bytes");
        assert_eq!(fs::read_to_string(dest.join("config/app.toml")).unwrap(), "key = 1\n");
        let read_back = read_manifest(&dest).unwrap();
        assert_eq!(read_back.project, manifest.project);
        assert_eq!(read_back.git_sha, manifest.git_sha);
        assert_eq!(read_back.binaries.len(), 1);
        assert_eq!(read_back.units[0].name, "app.service");
        verify_checksums(&dest, &read_back).unwrap();
    }

    #[test]
    fn extract_preserves_executable_mode() {
        let tmp = tempdir().unwrap();
        let (bundle, _) = build_sample_bundle(tmp.path());
        let dest = tmp.path().join("out");
        extract_bundle(&bundle, &dest).unwrap();
        let mode = fs::metadata(dest.join("bin/app")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn extract_creates_missing_destination() {
        let tmp = tempdir().unwrap();
        let (bundle, _) = build_sample_bundle(tmp.path());
        let dest = tmp.path().join("a/b/c");
        extract_bundle(&bundle, &dest).unwrap();
        assert!(dest.join("manifest.toml").is_file());
    }

    #[test]
    fn extract_rejects_missing_bundle_file() {
        let tmp = tempdir().unwrap();
        let err = extract_bundle(&tmp.path().join("nope.tar.zst"), &tmp.path().join("out"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("reading"));
    }

    #[test]
    fn extract_rejects_non_zstd_data() {
        let tmp = tempdir().unwrap();
        let bogus = tmp.path().join("bogus.tar.zst");
        fs::write(&bogus, b"definitely not zstd").unwrap();
        let err = extract_bundle(&bogus, &tmp.path().join("out")).unwrap_err();
        assert!(format!("{err:#}").contains("decompressing"));
    }

    #[test]
    fn extract_rejects_hardlink_entries() {
        let tmp = tempdir().unwrap();
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            header.set_mode(0o644);
            builder
                .append_link(&mut header, "innocuous", "other")
                .unwrap();
            builder.finish().unwrap();
        }
        let bundle = tmp.path().join("hardlink.tar.zst");
        fs::write(&bundle, zstd::stream::encode_all(buf.as_slice(), 3).unwrap()).unwrap();
        let err = extract_bundle(&bundle, &tmp.path().join("out")).unwrap_err();
        assert!(err.to_string().contains("link"));
    }

    #[test]
    fn read_manifest_missing_file_fails() {
        let tmp = tempdir().unwrap();
        assert!(read_manifest(tmp.path()).is_err());
    }

    #[test]
    fn read_manifest_malformed_toml_fails() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("manifest.toml"), "this is = = not toml").unwrap();
        assert!(read_manifest(tmp.path()).is_err());
    }

    #[test]
    fn read_manifest_rejects_unknown_schema_version() {
        let tmp = tempdir().unwrap();
        let mut manifest = sample_manifest(b"x", b"y");
        manifest.schema_version = CURRENT_SCHEMA_VERSION + 1;
        fs::write(
            tmp.path().join("manifest.toml"),
            toml::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let err = read_manifest(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }

    #[test]
    fn verify_checksums_accepts_uppercase_expected_hex() {
        let tmp = tempdir().unwrap();
        let mut manifest = write_staging(tmp.path());
        manifest.binaries[0].sha256 = manifest.binaries[0].sha256.to_uppercase();
        verify_checksums(tmp.path(), &manifest).unwrap();
    }

    #[test]
    fn verify_checksums_detects_tampered_binary() {
        let tmp = tempdir().unwrap();
        let manifest = write_staging(tmp.path());
        fs::write(tmp.path().join("bin/app"), b"tampered").unwrap();
        let err = verify_checksums(tmp.path(), &manifest).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn verify_checksums_detects_tampered_unit() {
        let tmp = tempdir().unwrap();
        let manifest = write_staging(tmp.path());
        fs::write(tmp.path().join("systemd/app.service"), b"[Service]\n").unwrap();
        let err = verify_checksums(tmp.path(), &manifest).unwrap_err();
        assert!(err.to_string().contains("app.service"));
    }

    #[test]
    fn extract_from_reader_matches_file_extract() {
        let tmp = tempdir().unwrap();
        let (bundle, _) = build_sample_bundle(tmp.path());
        let bytes = fs::read(&bundle).unwrap();
        let dest = tmp.path().join("out");
        extract_bundle_from_reader(bytes.as_slice(), &dest).unwrap();
        assert_eq!(fs::read(dest.join("bin/app")).unwrap(), b"binary bytes");
        verify_checksums(&dest, &read_manifest(&dest).unwrap()).unwrap();
    }

    #[test]
    fn extract_rejects_bundle_over_unpacked_limit() {
        let tmp = tempdir().unwrap();
        let (bundle, _) = build_sample_bundle(tmp.path());
        let bytes = fs::read(&bundle).unwrap();
        // The sample's tar stream is several KiB; 1 KiB must trip the cap.
        let err = extract_limited(bytes.as_slice(), &tmp.path().join("out"), 1024).unwrap_err();
        assert!(format!("{err:#}").contains("maximum unpacked size"), "{err:#}");
    }

    #[test]
    fn extract_accepts_bundle_exactly_at_unpacked_limit() {
        let tmp = tempdir().unwrap();
        let (bundle, _) = build_sample_bundle(tmp.path());
        let bytes = fs::read(&bundle).unwrap();
        let unpacked = zstd::stream::decode_all(bytes.as_slice()).unwrap().len() as u64;
        extract_limited(bytes.as_slice(), &tmp.path().join("out"), unpacked).unwrap();
    }

    fn write_manifest_with(dir: &Path, edit: impl FnOnce(&mut Manifest)) {
        let mut manifest = sample_manifest(b"x", b"y");
        edit(&mut manifest);
        fs::write(dir.join("manifest.toml"), toml::to_string_pretty(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn read_manifest_rejects_escaping_paths() {
        for bad in ["/etc/shadow", "../outside", "bin/../../outside", ""] {
            let tmp = tempdir().unwrap();
            write_manifest_with(tmp.path(), |m| m.binaries[0].path = bad.into());
            let err = read_manifest(tmp.path()).unwrap_err();
            assert!(err.to_string().contains("relative path inside the bundle"), "{bad}: {err}");

            let tmp = tempdir().unwrap();
            write_manifest_with(tmp.path(), |m| m.units[0].path = bad.into());
            assert!(read_manifest(tmp.path()).is_err(), "unit path {bad:?} accepted");
        }
    }

    #[test]
    fn read_manifest_rejects_unit_names_with_paths() {
        for bad in ["../x.service", "a/b.service", "/x.service", "..", ".", ""] {
            let tmp = tempdir().unwrap();
            write_manifest_with(tmp.path(), |m| m.units[0].name = bad.into());
            let err = read_manifest(tmp.path()).unwrap_err();
            assert!(err.to_string().contains("bare file name"), "{bad}: {err}");
        }
    }

    #[test]
    fn read_manifest_accepts_nested_relative_paths() {
        let tmp = tempdir().unwrap();
        write_manifest_with(tmp.path(), |m| m.binaries[0].path = "bin/sub/./app".into());
        read_manifest(tmp.path()).unwrap();
    }

    #[test]
    fn verify_checksums_fails_on_missing_file() {
        let tmp = tempdir().unwrap();
        let manifest = write_staging(tmp.path());
        fs::remove_file(tmp.path().join("bin/app")).unwrap();
        assert!(verify_checksums(tmp.path(), &manifest).is_err());
    }
}
