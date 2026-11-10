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
        Self {
            app_root: app_root.into(),
        }
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
        let format = time::format_description::parse_borrowed::<3>(
            "[year]-[month]-[day]T[hour]-[minute]-[second]Z",
        )
        .expect("static format string is valid");
        let ts = built_at
            .format(&format)
            .expect("formatting known-good timestamp");
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
            release_dir
                .file_name()
                .context("release_dir has no filename")?,
        );
        unix_fs::symlink(&relative_target, &tmp_link).context("creating temp symlink")?;
        fs::rename(&tmp_link, self.current_link()).context("renaming temp symlink over current")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn make_release(layout: &ReleaseLayout, name: &str) -> PathBuf {
        let p = layout.releases_dir().join(name);
        fs::create_dir_all(p.join("bin")).unwrap();
        p
    }

    #[test]
    fn release_dir_name_format() {
        let name = ReleaseLayout::new_release_dir_name(
            datetime!(2026-09-09 14:03:00 UTC),
            "0123456789abcdef0123",
        );
        assert_eq!(name, "2026-09-09T14-03-00Z_0123456789ab");
    }

    #[test]
    fn release_dir_name_keeps_short_sha() {
        let name = ReleaseLayout::new_release_dir_name(datetime!(2026-01-02 03:04:05 UTC), "abc");
        assert_eq!(name, "2026-01-02T03-04-05Z_abc");
    }

    #[test]
    fn layout_paths() {
        let layout = ReleaseLayout::new("/srv/apps/p");
        assert_eq!(layout.releases_dir(), Path::new("/srv/apps/p/releases"));
        assert_eq!(layout.current_link(), Path::new("/srv/apps/p/current"));
        assert_eq!(layout.shared_dir(), Path::new("/srv/apps/p/shared"));
        assert_eq!(
            layout.new_release_path(datetime!(2026-09-09 14:03:00 UTC), "abc"),
            Path::new("/srv/apps/p/releases/2026-09-09T14-03-00Z_abc")
        );
    }

    #[test]
    fn current_target_is_none_without_link() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        assert_eq!(layout.current_target().unwrap(), None);
    }

    #[test]
    fn swap_current_creates_relative_link() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let r1 = make_release(&layout, "r1");
        layout.swap_current(&r1).unwrap();

        assert_eq!(
            fs::read_link(layout.current_link()).unwrap(),
            Path::new("releases/r1")
        );
        assert_eq!(layout.current_target().unwrap(), Some(r1.clone()));
        assert!(layout.current_link().join("bin").is_dir());
        assert!(fs::symlink_metadata(tmp.path().join(".current.tmp")).is_err());
    }

    #[test]
    fn swap_current_replaces_existing_link() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let r1 = make_release(&layout, "r1");
        let r2 = make_release(&layout, "r2");
        layout.swap_current(&r1).unwrap();
        layout.swap_current(&r2).unwrap();
        assert_eq!(layout.current_target().unwrap(), Some(r2));
    }

    #[test]
    fn swap_current_cleans_stale_temp_link() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let r1 = make_release(&layout, "r1");
        unix_fs::symlink("releases/gone", tmp.path().join(".current.tmp")).unwrap();
        layout.swap_current(&r1).unwrap();
        assert_eq!(layout.current_target().unwrap(), Some(r1));
    }

    #[test]
    fn swap_current_rejects_path_without_file_name() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        assert!(layout.swap_current(Path::new("/")).is_err());
    }

    #[test]
    fn current_target_errors_when_current_is_not_a_link() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        fs::create_dir_all(layout.current_link()).unwrap();
        assert!(layout.current_target().is_err());
    }

    #[test]
    fn list_releases_sorted_and_dirs_only() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        make_release(&layout, "2026-01-03T00-00-00Z_c");
        make_release(&layout, "2026-01-01T00-00-00Z_a");
        make_release(&layout, "2026-01-02T00-00-00Z_b");
        fs::write(layout.releases_dir().join("stray-file"), b"").unwrap();

        let names: Vec<_> = layout
            .list_releases()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "2026-01-01T00-00-00Z_a",
                "2026-01-02T00-00-00Z_b",
                "2026-01-03T00-00-00Z_c"
            ]
        );
    }

    #[test]
    fn list_releases_errors_without_releases_dir() {
        let tmp = tempdir().unwrap();
        assert!(ReleaseLayout::new(tmp.path()).list_releases().is_err());
    }

    #[test]
    fn prune_noop_within_retention() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        make_release(&layout, "a");
        make_release(&layout, "b");
        assert!(layout.prune(2).unwrap().is_empty());
        assert_eq!(layout.list_releases().unwrap().len(), 2);
    }

    #[test]
    fn prune_removes_oldest_beyond_retention() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let a = make_release(&layout, "a");
        let b = make_release(&layout, "b");
        let c = make_release(&layout, "c");
        layout.swap_current(&c).unwrap();

        let removed = layout.prune(1).unwrap();
        assert_eq!(removed, vec![a.clone(), b.clone()]);
        assert!(!a.exists() && !b.exists() && c.exists());
    }

    #[test]
    fn prune_never_removes_current() {
        let tmp = tempdir().unwrap();
        let layout = ReleaseLayout::new(tmp.path());
        let a = make_release(&layout, "a");
        let b = make_release(&layout, "b");
        let c = make_release(&layout, "c");
        layout.swap_current(&a).unwrap(); // current is the oldest

        let removed = layout.prune(1).unwrap();
        assert_eq!(removed, vec![b.clone()]);
        assert!(a.exists() && !b.exists() && c.exists());
    }
}
