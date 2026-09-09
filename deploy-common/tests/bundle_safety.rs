use deploy_common::bundle;
use tempfile::tempdir;

// Build a raw malicious tar.zst by hand (bypassing build_bundle, which
// wouldn't produce these) and confirm extract_bundle refuses it.

#[test]
fn rejects_parent_traversal() {
    let bad = make_bundle_with_entry("../../etc/passwd", b"pwned");
    let dest = tempdir().unwrap();
    let result = bundle::extract_bundle(&bad, dest.path());
    assert!(result.is_err());
}

#[test]
fn rejects_absolute_path() {
    let bad = make_bundle_with_entry("/etc/passwd", b"pwned");
    let dest = tempdir().unwrap();
    let result = bundle::extract_bundle(&bad, dest.path());
    assert!(result.is_err());
}

#[test]
fn rejects_symlink_entries() {
    let bad = make_bundle_with_symlink("innocuous", "/etc/passwd");
    let dest = tempdir().unwrap();
    let result = bundle::extract_bundle(&bad, dest.path());
    assert!(result.is_err());
}

// --- helpers to hand-craft adversarial tar.zst files ---
fn make_bundle_with_entry(path: &str, contents: &[u8]) -> std::path::PathBuf {
    let dir = tempdir().unwrap();
    let out = dir.path().join("evil.tar.zst");
    let mut buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut buf);
        let mut header = tar::Header::new_gnu();

        // Bypass Header::set_path's validation (which would reject
        // ".." / absolute paths) by writing the raw name field directly —
        // we WANT an archive that's malicious at this level, to prove
        // our own extract_bundle catches it independently of tar-rs's
        // own guard.
        {
            let gnu = header.as_gnu_mut().unwrap();
            let name_bytes = path.as_bytes();
            gnu.name[..name_bytes.len()].copy_from_slice(name_bytes);
        }
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();

        // append() (not append_data()) does no path validation.
        builder.append(&header, contents).unwrap();
        builder.finish().unwrap();
    }
    let compressed = zstd::stream::encode_all(buf.as_slice(), 3).unwrap();
    std::fs::write(&out, compressed).unwrap();
    std::mem::forget(dir);
    out
}

fn make_bundle_with_symlink(path: &str, target: &str) -> std::path::PathBuf {
    let dir = tempdir().unwrap();
    let out = dir.path().join("evil.tar.zst");
    let mut buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut buf);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_link(&mut header, path, target).unwrap();
        builder.finish().unwrap();
    }
    let compressed = zstd::stream::encode_all(buf.as_slice(), 3).unwrap();
    std::fs::write(&out, compressed).unwrap();
    std::mem::forget(dir);
    out
}
