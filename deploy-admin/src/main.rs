use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use clap::Parser;
use deploy_common::project::{HealthCheckConfig, ProjectConfig};

/// Onboarding tool for deploy-agent. Runs on the Pi, by hand, under sudo —
/// never invoked by the deploy pipeline itself. Writes the per-project
/// policy/secret deploy-agent trusts, but only ever PRINTS the polkit
/// snippet rather than writing it: that file stays reviewed, rarely-changed,
/// root-owned config a human edits by hand.
#[derive(Debug, Parser)]
#[command(name = "deploy-admin", about = "Project onboarding for deploy-agent")]
struct Args {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Debug, clap::Subcommand)]
enum Command_ {
    /// Create (or update) a project's config, secret, and install directory.
    /// Safe to re-run: an existing secret is preserved unless --force is
    /// passed, and the project TOML / directories are recomputed each time.
    Onboard {
        /// Project name — matches X-Deploy-Project and the bundle's
        /// manifest.project.
        project: String,

        /// Systemd unit names this project's manifest may touch, comma
        /// separated (e.g. api.service,worker.service). Also enforced by
        /// the polkit rule this command prints a snippet for.
        #[arg(long, value_delimiter = ',')]
        units: Vec<String>,

        /// Where releases/current/shared live for this project. Defaults
        /// to /srv/apps/<project> (or pass /opt/deploy-agent explicitly
        /// when onboarding the "deploy-agent" self-project).
        #[arg(long)]
        install_dir: Option<PathBuf>,

        #[arg(long, default_value_t = 5)]
        retain_count: usize,

        #[arg(long)]
        health_check_url: Option<String>,

        #[arg(long, default_value_t = 10)]
        health_check_timeout_secs: u64,

        /// Root containing projects/ and secrets/. Defaults to
        /// $DEPLOY_AGENT_CONFIG_ROOT or /etc/deploy-agent — same
        /// convention deploy-agent itself uses, so this can be pointed at
        /// a scratch directory for local testing without touching /etc.
        #[arg(long)]
        config_root: Option<PathBuf>,

        /// System user deploy-agent runs as; install_dir and the
        /// project's secret/config are chowned to this user (best effort
        /// — requires running as root, which is the expected sudo case).
        #[arg(long, default_value = "deploy-agent")]
        agent_user: String,

        #[arg(long, default_value = "deploy-agent")]
        agent_group: String,

        /// Regenerate the HMAC secret even if one already exists.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command_::Onboard {
            project,
            units,
            install_dir,
            retain_count,
            health_check_url,
            health_check_timeout_secs,
            config_root,
            agent_user,
            agent_group,
            force,
        } => onboard(
            &project,
            units,
            install_dir,
            retain_count,
            health_check_url,
            health_check_timeout_secs,
            config_root,
            &agent_user,
            &agent_group,
            force,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn onboard(
    project: &str,
    units: Vec<String>,
    install_dir: Option<PathBuf>,
    retain_count: usize,
    health_check_url: Option<String>,
    health_check_timeout_secs: u64,
    config_root: Option<PathBuf>,
    agent_user: &str,
    agent_group: &str,
    force: bool,
) -> Result<()> {
    let config_root = config_root
        .or_else(|| std::env::var_os("DEPLOY_AGENT_CONFIG_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/etc/deploy-agent"));
    let install_dir =
        install_dir.unwrap_or_else(|| PathBuf::from(format!("/srv/apps/{project}")));

    let secrets_dir = config_root.join("secrets");
    let projects_dir = config_root.join("projects");
    fs::create_dir_all(&secrets_dir)
        .with_context(|| format!("creating {}", secrets_dir.display()))?;
    fs::create_dir_all(&projects_dir)
        .with_context(|| format!("creating {}", projects_dir.display()))?;

    let secret_path = secrets_dir.join(format!("{project}.key"));
    let project_toml_path = projects_dir.join(format!("{project}.toml"));

    // --- 1. Secret: generate unless one already exists (idempotent) ---
    if force || !secret_path.exists() {
        write_new_secret(&secret_path)?;
        println!("wrote new HMAC secret: {}", secret_path.display());
    } else {
        println!("secret already exists, left untouched (pass --force to regenerate): {}", secret_path.display());
    }

    // --- 2. Project policy TOML — always recomputed from the given args ---
    let health_check = health_check_url.map(|url| HealthCheckConfig {
        url,
        timeout_secs: health_check_timeout_secs,
    });
    let project_config = ProjectConfig {
        install_dir: install_dir.clone(),
        allowed_units: units.clone(),
        retain_count,
        health_check,
        secret_path: secret_path.clone(),
    };
    let toml_body = toml::to_string_pretty(&project_config)
        .context("serializing project config to TOML")?;
    fs::write(&project_toml_path, toml_body)
        .with_context(|| format!("writing {}", project_toml_path.display()))?;
    println!("wrote project config: {}", project_toml_path.display());

    // --- 3. Install directory scaffolding ---
    let releases_dir = install_dir.join("releases");
    let shared_dir = install_dir.join("shared");
    fs::create_dir_all(&releases_dir)
        .with_context(|| format!("creating {}", releases_dir.display()))?;
    fs::create_dir_all(&shared_dir)
        .with_context(|| format!("creating {}", shared_dir.display()))?;
    println!("ensured install directories under {}", install_dir.display());

    // --- 4. Best-effort ownership so deploy-agent (a non-root user) can
    //         write releases and read its own secret/config without sudo.
    //         Requires this tool itself to be running as root; if it
    //         isn't, warn rather than aborting so onboarding can still be
    //         exercised locally during development. ---
    let owner = format!("{agent_user}:{agent_group}");
    chown_best_effort(&install_dir, &owner);
    chown_best_effort(&secret_path, &owner);
    chown_best_effort(&project_toml_path, &owner);

    // --- 5. Print (never write) the polkit snippet for this project's units ---
    if units.is_empty() {
        println!(
            "\nno units given — nothing to add to the polkit allow-list for '{project}'."
        );
    } else {
        println!(
            "\nAdd these units to the allowedUnits array in \
             /etc/polkit-1/rules.d/49-deploy-agent.rules (polkit picks up \
             rule-file changes automatically, no daemon-reload needed):\n"
        );
        for unit in &units {
            println!("    \"{unit}\",");
        }
    }

    Ok(())
}

fn write_new_secret(path: &Path) -> Result<()> {
    let mut key_bytes = [0u8; 32];
    rand::fill(&mut key_bytes);
    let hex_key = hex::encode(key_bytes);
    fs::write(path, hex_key).with_context(|| format!("writing secret {}", path.display()))?;
    set_mode(path, 0o600)
        .with_context(|| format!("setting mode 600 on {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Shells out to `chown` rather than pulling in a uid/gid-resolving crate
/// for this one admin-only operation. Best effort: a failure (e.g. this
/// tool not running as root, or the user/group not existing yet) is
/// printed as a warning, not a hard error — deploy-admin is meant to be
/// run under sudo on the Pi, but shouldn't be unusable for local dry runs.
fn chown_best_effort(path: &Path, owner: &str) {
    match Command::new("chown").arg("-R").arg(owner).arg(path).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "warning: chown {owner} {} failed: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("warning: could not run chown for {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{TempDir, tempdir};

    struct Run {
        _tmp: TempDir,
        config_root: PathBuf,
        install_dir: PathBuf,
    }

    impl Run {
        fn new() -> Self {
            let tmp = tempdir().unwrap();
            Self {
                config_root: tmp.path().join("etc"),
                install_dir: tmp.path().join("srv/myproj"),
                _tmp: tmp,
            }
        }

        fn onboard(&self, units: &[&str], health: Option<&str>, force: bool) -> Result<()> {
            onboard(
                "myproj",
                units.iter().map(|s| s.to_string()).collect(),
                Some(self.install_dir.clone()),
                3,
                health.map(String::from),
                7,
                Some(self.config_root.clone()),
                "deploy-agent",
                "deploy-agent",
                force,
            )
        }

        fn secret(&self) -> String {
            fs::read_to_string(self.secret_path()).unwrap()
        }

        fn secret_path(&self) -> PathBuf {
            self.config_root.join("secrets/myproj.key")
        }

        fn config(&self) -> ProjectConfig {
            ProjectConfig::load(&self.config_root.join("projects/myproj.toml")).unwrap()
        }
    }

    #[test]
    fn onboard_creates_secret_config_and_dirs() {
        let run = Run::new();
        run.onboard(&["a.service", "b.service"], None, false).unwrap();

        let secret = run.secret();
        assert_eq!(secret.len(), 64);
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        let mode = fs::metadata(run.secret_path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let cfg = run.config();
        assert_eq!(cfg.install_dir, run.install_dir);
        assert_eq!(cfg.allowed_units, ["a.service", "b.service"]);
        assert_eq!(cfg.retain_count, 3);
        assert_eq!(cfg.secret_path, run.secret_path());
        assert!(cfg.health_check.is_none());

        assert!(run.install_dir.join("releases").is_dir());
        assert!(run.install_dir.join("shared").is_dir());
    }

    #[test]
    fn onboard_writes_health_check() {
        let run = Run::new();
        run.onboard(&[], Some("http://127.0.0.1:9000/healthz"), false).unwrap();
        let hc = run.config().health_check.unwrap();
        assert_eq!(hc.url, "http://127.0.0.1:9000/healthz");
        assert_eq!(hc.timeout_secs, 7);
    }

    #[test]
    fn onboard_without_units_is_allowed() {
        let run = Run::new();
        run.onboard(&[], None, false).unwrap();
        assert!(run.config().allowed_units.is_empty());
    }

    #[test]
    fn rerun_preserves_secret_and_updates_config() {
        let run = Run::new();
        run.onboard(&["a.service"], None, false).unwrap();
        let first = run.secret();

        run.onboard(&["a.service", "c.service"], None, false).unwrap();

        assert_eq!(run.secret(), first);
        assert_eq!(run.config().allowed_units, ["a.service", "c.service"]);
    }

    #[test]
    fn force_regenerates_secret() {
        let run = Run::new();
        run.onboard(&[], None, false).unwrap();
        let first = run.secret();
        run.onboard(&[], None, true).unwrap();
        assert_ne!(run.secret(), first);
        assert_eq!(run.secret().len(), 64);
    }

    #[test]
    fn rerun_keeps_existing_releases() {
        let run = Run::new();
        run.onboard(&[], None, false).unwrap();
        fs::create_dir_all(run.install_dir.join("releases/r1")).unwrap();
        run.onboard(&[], None, false).unwrap();
        assert!(run.install_dir.join("releases/r1").is_dir());
    }

    #[test]
    fn onboard_fails_when_config_root_is_a_file() {
        let run = Run::new();
        fs::create_dir_all(run.config_root.parent().unwrap()).unwrap();
        fs::write(&run.config_root, b"not a dir").unwrap();
        let err = run.onboard(&[], None, false).unwrap_err();
        assert!(format!("{err:#}").contains("creating"));
    }

    #[test]
    fn onboard_fails_when_install_dir_is_a_file() {
        let run = Run::new();
        fs::create_dir_all(run.install_dir.parent().unwrap()).unwrap();
        fs::write(&run.install_dir, b"not a dir").unwrap();
        assert!(run.onboard(&[], None, false).is_err());
    }

    #[test]
    fn write_new_secret_generates_distinct_keys() {
        let tmp = tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        write_new_secret(&a).unwrap();
        write_new_secret(&b).unwrap();
        assert_ne!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
    }

    #[test]
    fn write_new_secret_fails_in_missing_dir() {
        let tmp = tempdir().unwrap();
        assert!(write_new_secret(&tmp.path().join("no/such/key")).is_err());
    }

    #[test]
    fn set_mode_applies_and_fails_on_missing_file() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("f");
        fs::write(&p, b"").unwrap();
        set_mode(&p, 0o640).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(set_mode(&tmp.path().join("missing"), 0o600).is_err());
    }

    #[test]
    fn chown_best_effort_never_panics() {
        let tmp = tempdir().unwrap();
        chown_best_effort(&tmp.path().join("missing"), "no-such-user:no-such-group");
    }
}
