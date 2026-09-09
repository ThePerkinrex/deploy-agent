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
