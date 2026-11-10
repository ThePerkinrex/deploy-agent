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
    /// Unix mode bits of the staged file (e.g. 0o755), recorded by
    /// deploy-ci. Informational only: deploy-agent does not apply it —
    /// the extracted file keeps the mode stored in the tar header.
    #[serde(default = "default_exec_mode")]
    pub mode: u32,
}

const fn default_exec_mode() -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
schema_version = 1
project = "p"
git_sha = "abc"
built_at = "2026-09-09T14:03:00Z"

[[binaries]]
path = "bin/app"
sha256 = "00"

[[units]]
name = "app.service"
path = "systemd/app.service"
sha256 = "11"
"#;

    #[test]
    fn parses_minimal_manifest_with_defaults() {
        let m: Manifest = toml::from_str(MINIMAL).unwrap();
        assert_eq!(m.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(m.binaries[0].mode, 0o755);
        assert!(m.build_meta.is_empty());
        assert_eq!(m.built_at, time::macros::datetime!(2026-09-09 14:03:00 UTC));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut m: Manifest = toml::from_str(MINIMAL).unwrap();
        m.build_meta.insert("branch".into(), "main".into());
        m.binaries[0].mode = 0o700;
        let back: Manifest = toml::from_str(&toml::to_string_pretty(&m).unwrap()).unwrap();
        assert_eq!(back.build_meta["branch"], "main");
        assert_eq!(back.binaries[0].mode, 0o700);
        assert_eq!(back.built_at, m.built_at);
        assert_eq!(back.units[0].name, "app.service");
    }

    #[test]
    fn rejects_non_rfc3339_built_at() {
        let bad = MINIMAL.replace("2026-09-09T14:03:00Z", "yesterday");
        assert!(toml::from_str::<Manifest>(&bad).is_err());
    }

    #[test]
    fn rejects_missing_required_field() {
        let bad = MINIMAL.replace("git_sha = \"abc\"\n", "");
        assert!(toml::from_str::<Manifest>(&bad).is_err());
    }

    #[test]
    fn units_are_required() {
        let bad = MINIMAL.split("[[units]]").next().unwrap();
        assert!(toml::from_str::<Manifest>(bad).is_err());
    }
}
