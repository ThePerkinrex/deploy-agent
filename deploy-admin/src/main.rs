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
