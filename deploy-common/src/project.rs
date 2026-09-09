use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// e.g. /srv/apps/myproj  (or /opt/deploy-agent for the self project)
    pub install_dir: PathBuf,

    /// Systemd unit names this agent may touch for this project. Enforced
    /// again at the OS level by the polkit rule (Step 4) — this is a
    /// second, defense-in-depth check at the application layer.
    #[serde(default)]
    pub allowed_units: Vec<String>,

    #[serde(default = "default_retain_count")]
    pub retain_count: usize,

    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,

    /// Path to this project's HMAC secret file, e.g.
    /// /etc/deploy-agent/secrets/myproj.key
    pub secret_path: PathBuf,
}

const fn default_retain_count() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub url: String,
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
}

const fn default_health_timeout() -> u64 {
    10
}

impl ProjectConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading project config {}: {e}", path.display()))?;
        Ok(toml::from_str(&raw)?)
    }
}
