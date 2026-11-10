use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[cfg(test)]
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn load_str(contents: &str) -> anyhow::Result<ProjectConfig> {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("p.toml");
        std::fs::write(&p, contents).unwrap();
        ProjectConfig::load(&p)
    }

    #[test]
    fn loads_full_config() {
        let c = load_str(
            r#"
install_dir = "/srv/apps/p"
allowed_units = ["a.service", "b.service"]
retain_count = 3
secret_path = "/etc/deploy-agent/secrets/p.key"

[health_check]
url = "http://127.0.0.1:9000/healthz"
timeout_secs = 4
"#,
        )
        .unwrap();
        assert_eq!(c.install_dir, PathBuf::from("/srv/apps/p"));
        assert_eq!(c.allowed_units, ["a.service", "b.service"]);
        assert_eq!(c.retain_count, 3);
        let hc = c.health_check.unwrap();
        assert_eq!(hc.url, "http://127.0.0.1:9000/healthz");
        assert_eq!(hc.timeout_secs, 4);
    }

    #[test]
    fn applies_defaults() {
        let c = load_str(
            r#"
install_dir = "/srv/apps/p"
secret_path = "/k"

[health_check]
url = "http://x"
"#,
        )
        .unwrap();
        assert!(c.allowed_units.is_empty());
        assert_eq!(c.retain_count, 5);
        assert_eq!(c.health_check.unwrap().timeout_secs, 10);
    }

    #[test]
    fn health_check_is_optional() {
        let c = load_str("install_dir = \"/a\"\nsecret_path = \"/k\"\n").unwrap();
        assert!(c.health_check.is_none());
    }

    #[test]
    fn missing_file_error_names_path() {
        let err = ProjectConfig::load(Path::new("/nonexistent/p.toml")).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/p.toml"));
    }

    #[test]
    fn missing_required_field_fails() {
        assert!(load_str("install_dir = \"/a\"\n").is_err());
        assert!(load_str("secret_path = \"/k\"\n").is_err());
    }

    #[test]
    fn wrong_type_fails() {
        assert!(load_str("install_dir = \"/a\"\nsecret_path = \"/k\"\nretain_count = \"five\"\n").is_err());
    }

    #[test]
    fn serialize_round_trip() {
        let c = load_str("install_dir = \"/a\"\nsecret_path = \"/k\"\nallowed_units = [\"x.service\"]\n").unwrap();
        let again: ProjectConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(again.allowed_units, c.allowed_units);
        assert_eq!(again.install_dir, c.install_dir);
    }
}
