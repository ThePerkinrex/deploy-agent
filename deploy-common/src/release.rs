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
