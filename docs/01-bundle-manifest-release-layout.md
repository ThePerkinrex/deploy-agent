# Step 1: Bundle Format, Manifest Schema, Release Layout

This is the foundation everything else builds on — get this right before writing
any networking, D-Bus, or systemd code. Nothing here talks to the Pi yet; you're
just defining the data shapes and directory conventions, and writing small
standalone Rust binaries/tests to prove they round-trip correctly.

Goal by the end of this step: you can construct a bundle on your dev machine,
extract it into a scratch directory, and produce the exact `releases/<ts>_<sha>/`
layout described in the handoff — with tests, no server involved yet.

---

## 1.1 — Set up the workspace

You said this deploys a Rust workspace with two binary crates. `deploy-agent`
itself should live as a third crate in the *same* workspace, since (per the
handoff) it deploys itself through the same mechanism and should eventually be
"just another project" in config.

```bash
cargo new --lib deploy-agent-workspace   # or reuse your existing workspace root
cd deploy-agent-workspace
```

Workspace `Cargo.toml` at the root:

```toml
[workspace]
resolver = "2"
members = [
    "deploy-agent",
    "deploy-common",
]
```

Create two crates for this step:

```bash
cargo new deploy-common --lib   # shared types: Manifest, bundle read/write, release layout
cargo new deploy-agent  --bin   # will grow into the server later; empty-ish for now
```

Everything in this step goes in `deploy-common`, because both the CI-side
"bundle builder" tool and the on-Pi agent need the exact same `Manifest`
struct and the exact same layout logic. Don't duplicate this later — CI will
eventually use `deploy-common` too (as a `cargo xtask` or small CLI, see 1.5).

`deploy-common/Cargo.toml`:

```toml
[package]
name = "deploy-common"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
tar = "0.4"
zstd = "0.13"
sha2 = "0.10"
anyhow = "1"
thiserror = "1"
walkdir = "2"
time = { version = "0.3", features = ["formatting", "parsing", "macros"] }

[dev-dependencies]
tempfile = "3"
```

---

## 1.2 — The manifest schema (with versioning from day one)

Open question #2 from the handoff explicitly flags: version the schema now so
future manifest changes don't break older agent versions. The cheap way to do
this that doesn't require a full schema-registry: an explicit `schema_version`
field that the agent switch/matches on, plus `#[serde(default)]` on anything
optional so old manifests still parse against a newer struct.

`deploy-common/src/manifest.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bump this only on breaking changes to the manifest shape.
/// Additive, backward-compatible fields do NOT require a bump —
/// just add them with #[serde(default)].
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,

    /// Which project this bundle belongs to. Must match the
    /// X-Deploy-Project header and the server-side project config name —
    /// the agent should reject a mismatch rather than trust the header alone.
    pub project: String,

    /// Full git commit sha the bundle was built from.
    pub git_sha: String,

    /// RFC 3339 UTC timestamp of the build, set by CI.
    #[serde(with = "time::serde::rfc3339")]
    pub built_at: time::OffsetDateTime,

    /// Optional free-form build metadata (branch, workflow run id, etc).
    /// Never load-bearing for deploy logic — purely informational.
    #[serde(default)]
    pub build_meta: BTreeMap<String, String>,

    pub binaries: Vec<BinaryEntry>,
    pub units: Vec<UnitEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryEntry {
    /// Path relative to bundle root, e.g. "bin/myproj-api"
    pub path: String,
    /// hex-encoded sha256 of the file contents, checked after extraction
    /// and again before the atomic symlink swap.
    pub sha256: String,
    /// Unix mode bits to apply after extraction (e.g. 0o755).
    #[serde(default = "default_exec_mode")]
    pub mode: u32,
}

fn default_exec_mode() -> u32 {
    0o755
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitEntry {
    /// Unit filename, e.g. "myproj-api.service"
    pub name: String,
    /// Path relative to bundle root, e.g. "systemd/myproj-api.service"
    pub path: String,
    /// sha256 of the unit file, same reasoning as binaries.
    pub sha256: String,
}
```

Why these choices:

- `schema_version` is checked explicitly in code (`match manifest.schema_version { 1 => ..., v => bail!("unsupported manifest schema version {v}") }`), not just "does it parse." A manifest that *parses* under a newer struct but was built assuming different semantics is worse than one that fails loudly.
- `sha256` lives on every binary and unit *individually*, not just as one bundle-level hash — this lets the agent verify each file after extraction and gives you a precise error ("binary myproj-api failed checksum") instead of "bundle corrupt."
- `build_meta` is a deliberate escape hatch: free-form key/value so you can stuff in `workflow_run_id`, `branch`, `triggered_by` without a schema bump. Never branch deploy logic on its contents.

---

## 1.3 — Bundle read/write

`deploy-common/src/bundle.rs` — this is used by both the CI-side builder and
the agent's unpack step, so the archive-safety logic (path sanitization) only
needs to be written once and tested once.

```rust
use crate::manifest::Manifest;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

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
        if entry.header().entry_type().is_symlink()
            || entry.header().entry_type().is_hard_link()
        {
            bail!("bundle entry is a link, refusing: {:?}", path);
        }

        let target = dest_dir.join(&path);
        // Belt-and-suspenders: after joining, confirm the resolved parent
        // is still inside dest_dir.
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
            let parent_canon = parent.canonicalize()
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
    let mut f = fs::File::open(path)
        .with_context(|| format!("opening {} for checksum", path.display()))?;
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
```

Add `hex = "0.4"` to `deploy-common`'s `[dependencies]`.

Note the path-sanitization approach: reject on **component inspection**
(no `..`, no absolute), reject **all symlinks/hardlinks** outright (simplest
safe policy — this bundle format never needs them), and re-canonicalize
after joining as a second check. Three independent layers because tar-slip
bugs are exactly the kind of thing worth being paranoid about.

---

## 1.4 — Release directory layout

`deploy-common/src/release.rs` — this owns the `releases/<ts>_<sha>/`
naming convention and the atomic symlink swap, so the agent later just calls
into this rather than reimplementing path-joining logic inline.

```rust
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

pub struct ReleaseLayout {
    /// e.g. /srv/apps/myproj  (or /opt/deploy-agent for self)
    pub app_root: PathBuf,
}

impl ReleaseLayout {
    pub fn new(app_root: impl Into<PathBuf>) -> Self {
        Self { app_root: app_root.into() }
    }

    pub fn releases_dir(&self) -> PathBuf {
        self.app_root.join("releases")
    }

    pub fn current_link(&self) -> PathBuf {
        self.app_root.join("current")
    }

    pub fn shared_dir(&self) -> PathBuf {
        self.app_root.join("shared")
    }

    /// Directory name format: 2026-09-09T14-03-00Z_<12-char-sha-prefix>
    /// Colons are avoided (filesystem-hostile on some setups) — hyphens
    /// throughout instead, still lexically sortable.
    pub fn new_release_dir_name(built_at: time::OffsetDateTime, git_sha: &str) -> String {
        let format = time::format_description::parse(
            "[year]-[month]-[day]T[hour]-[minute]-[second]Z"
        ).expect("static format string is valid");
        let ts = built_at.format(&format).expect("formatting known-good timestamp");
        let sha_prefix = &git_sha[..git_sha.len().min(12)];
        format!("{ts}_{sha_prefix}")
    }

    pub fn new_release_path(&self, built_at: time::OffsetDateTime, git_sha: &str) -> PathBuf {
        self.releases_dir()
            .join(Self::new_release_dir_name(built_at, git_sha))
    }

    /// Atomically point `current` at `release_dir`. Uses the
    /// symlink-to-temp-name-then-rename trick so there is never a moment
    /// where `current` doesn't exist or points at a half-written target.
    pub fn swap_current(&self, release_dir: &Path) -> Result<()> {
        let tmp_link = self.app_root.join(".current.tmp");
        if tmp_link.exists() || tmp_link.symlink_metadata().is_ok() {
            fs::remove_file(&tmp_link).ok();
        }
        // Relative symlink target keeps this portable if app_root is ever
        // moved/bind-mounted, and matches Capistrano convention.
        let relative_target = Path::new("releases").join(
            release_dir.file_name().context("release_dir has no filename")?
        );
        unix_fs::symlink(&relative_target, &tmp_link)
            .context("creating temp symlink")?;
        fs::rename(&tmp_link, self.current_link())
            .context("renaming temp symlink over current")?;
        Ok(())
    }

    /// Returns the release dir `current` points at, if any.
    pub fn current_target(&self) -> Result<Option<PathBuf>> {
        match fs::read_link(self.current_link()) {
            Ok(rel) => Ok(Some(self.app_root.join(rel))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Lists release directories oldest-first, by name (which sorts
    /// chronologically thanks to the timestamp prefix).
    pub fn list_releases(&self) -> Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = fs::read_dir(self.releases_dir())?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();
        entries.sort();
        Ok(entries)
    }

    /// Deletes releases beyond `retain_count`, oldest first, NEVER deleting
    /// whatever `current` currently points at even if retention math would
    /// otherwise include it (defensive — shouldn't happen if swap always
    /// precedes prune, but cheap to guard).
    pub fn prune(&self, retain_count: usize) -> Result<Vec<PathBuf>> {
        let releases = self.list_releases()?;
        let current = self.current_target()?;
        let mut removed = Vec::new();

        if releases.len() <= retain_count {
            return Ok(removed);
        }
        let excess = releases.len() - retain_count;
        for old in releases.into_iter().take(excess) {
            if Some(&old) == current.as_ref() {
                continue;
            }
            fs::remove_dir_all(&old)?;
            removed.push(old);
        }
        Ok(removed)
    }
}
```

---

## 1.5 — A CLI to exercise this by hand (no server yet)

Small `deploy-common` example binary so you can prove the round trip works
before any networking exists. This will later evolve into (or be reused by)
the CI-side bundle-builder step.

`deploy-common/examples/bundle_roundtrip.rs`:

```rust
use deploy_common::{bundle, manifest::{Manifest, BinaryEntry, UnitEntry, CURRENT_SCHEMA_VERSION}, release::ReleaseLayout};
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
        binaries: vec![BinaryEntry { path: "bin/myproj-api".into(), sha256: bin_sha, mode: 0o755 }],
        units: vec![UnitEntry { name: "myproj-api.service".into(), path: "systemd/myproj-api.service".into(), sha256: unit_sha }],
    };
    fs::write(src.path().join("manifest.toml"), toml::to_string_pretty(&manifest)?)?;

    // 2. Build bundle
    let bundle_path = src.path().join("../bundle.tar.zst");
    bundle::build_bundle(src.path(), &bundle_path)?;
    println!("bundle written: {} ({} bytes)", bundle_path.display(), fs::metadata(&bundle_path)?.len());

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
```

Run it:

```bash
cargo run --example bundle_roundtrip -p deploy-common
```

Expected output: bundle size printed, then `current -> Some(...)` pointing
at the new release dir, then `OK`. If any checksum or path-sanitization
check trips, you'll get an `anyhow` error with context — good, that's the
point of this step.

Then write the negative-path tests — these matter more than the happy path:

`deploy-common/tests/bundle_safety.rs`:

```rust
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
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, contents).unwrap();
        builder.finish().unwrap();
    }
    let compressed = zstd::stream::encode_all(buf.as_slice(), 3).unwrap();
    std::fs::write(&out, compressed).unwrap();
    // leak the tempdir so the file survives past this function
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
```

```bash
cargo test -p deploy-common
```

All three should fail extraction (i.e. the tests should pass because
`extract_bundle` correctly errors out).

---

## 1.6 — Exit criteria for this step

Before moving to Step 2 (minimal HTTP agent), confirm:

- [ ] `cargo run --example bundle_roundtrip -p deploy-common` succeeds end to end
- [ ] `cargo test -p deploy-common` passes, including the three adversarial-path tests
- [ ] You've eyeballed the actual bytes of a built bundle (`tar --zstd -tvf bundle.tar.zst`) and it matches the `manifest.toml / bin/ / systemd/ / config/` layout from the handoff
- [ ] `swap_current` produces a *relative* symlink (`readlink current` → `releases/2026-...`) — check this by hand, since an absolute symlink would break if `/srv/apps/<project>` were ever bind-mounted elsewhere
- [ ] `prune()` never deletes whatever `current` points at — write a quick manual test: swap to release A, prune with retain_count=0, confirm A survives

Once these hold, move to **Step 2: minimal agent — single endpoint, HMAC
verify, unpack → stage → symlink swap (no systemd yet)**. That step will
wire `deploy-common` into an `axum` server and add the HMAC signature
verification described in the handoff (`timestamp + "\n" + project + "\n" +
sha256(body)`, 5-minute replay window, constant-time compare).

Want me to write that guide next, in the same style?
