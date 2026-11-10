//! Real systemd, opt-in: `cargo test -p integration-tests --features systemd-tests`.
//!
//! The agent is pointed at the *user* manager (`systemd --user`) by setting
//! its `DBUS_SYSTEM_BUS_ADDRESS` to the session bus. That keeps it away from
//! PID 1 entirely:
//! - no root, no polkit; the user manager can only run processes as you;
//! - unit names are unique per run (`deploy-agent-it-<pid>-<n>-*.service`);
//! - units are `Type=oneshot` shell scripts that append a line to a file in
//!   the test's temp dir, nothing long-running, no ports;
//! - the only thing written outside the temp dir is one symlink per unit in
//!   `$XDG_RUNTIME_DIR/systemd/user/` (a tmpfs, wiped at logout), standing
//!   in for the generator; a guard stops the units, removes the links and
//!   daemon-reloads on drop, even when a test panics.
//!
//! For full isolation (or on CI without a user session) run the suite in a
//! throwaway container/VM with systemd as PID 1; see the README.
#![cfg(feature = "systemd-tests")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use integration_tests::{Agent, Env, Project, read, sha};

struct UserSystemd {
    bus: String,
    unit_dir: PathBuf,
    units: Vec<String>,
}

impl UserSystemd {
    fn connect() -> Self {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .expect("systemd-tests need a user session (XDG_RUNTIME_DIR is unset)");
        let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .unwrap_or_else(|_| format!("unix:path={runtime}/bus"));
        let state = systemctl(&["is-system-running"]);
        assert!(
            ["running", "degraded", "starting"].contains(&state.as_str()),
            "user systemd manager not usable (is-system-running: {state:?})"
        );
        let unit_dir = PathBuf::from(runtime).join("systemd/user");
        fs::create_dir_all(&unit_dir).unwrap();
        Self { bus, unit_dir, units: Vec::new() }
    }

    /// A fresh unit name, symlinked from the user runtime unit dir to where
    /// the agent will write it (what the generator does on the Pi).
    fn unit(&mut self, env: &Env, project: &str) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "deploy-agent-it-{}-{}-app.service",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let target = env.config_root.join("units").join(project).join(&name);
        std::os::unix::fs::symlink(&target, self.unit_dir.join(&name)).unwrap();
        self.units.push(name.clone());
        systemctl(&["daemon-reload"]);
        name
    }

    fn start_agent(&self, env: &Env) -> Agent {
        env.start_agent_with(&[("DBUS_SYSTEM_BUS_ADDRESS", &self.bus)])
    }
}

impl Drop for UserSystemd {
    fn drop(&mut self) {
        if self.units.is_empty() {
            return;
        }
        let mut stop = vec!["stop"];
        stop.extend(self.units.iter().map(String::as_str));
        systemctl(&stop);
        for u in &self.units {
            let _ = fs::remove_file(self.unit_dir.join(u));
        }
        systemctl(&["daemon-reload"]);
        let mut reset = vec!["reset-failed"];
        reset.extend(self.units.iter().map(String::as_str));
        systemctl(&reset);
    }
}

fn systemctl(args: &[&str]) -> String {
    let out = Command::new("systemctl").arg("--user").args(args).output().expect("running systemctl");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A oneshot unit running `current/bin/app` with `VERSION` set.
fn unit_file(proj: &Project, version: &str) -> String {
    format!(
        "[Unit]\nDescription=deploy-agent integration test (safe to delete)\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nEnvironment=VERSION={version}\n\
         ExecStart={}/current/bin/app\n",
        proj.install_dir.display()
    )
}

/// Appends "<tag> $VERSION" to `log`, then exits with `code`.
fn app(log: &std::path::Path, tag: char, code: i32) -> String {
    format!("#!/bin/sh\necho \"{tag} $VERSION\" >> '{}'\nexit {code}\n", log.display())
}

struct Case {
    // Drop order: agent, then unit cleanup, then the temp dir.
    agent: Agent,
    _sd: UserSystemd,
    proj: Project,
    unit: String,
    log: PathBuf,
    env: Env,
}

fn case() -> Case {
    let env = Env::new();
    let mut sd = UserSystemd::connect();
    // Name must be known before onboarding, since it goes on the allow-list.
    let unit = sd.unit(&env, "webapp");
    let proj = env.onboard("webapp", &[&unit], 5);
    let agent = sd.start_agent(&env);
    let log = env.path().join("ran.log");
    Case { agent, _sd: sd, proj, unit, log, env }
}

impl Case {
    fn deploy(&self, tag: char, version: &str, exit_code: i32) -> integration_tests::SendResult {
        let bundle = self
            .env
            .staging()
            .bin("app", &app(&self.log, tag, exit_code), 0o755)
            .unit(&self.unit, &unit_file(&self.proj, version))
            .bundle("webapp", &sha(tag));
        self.env.send(&self.agent, &self.proj, &bundle)
    }

    fn runs(&self) -> Vec<String> {
        if !self.log.exists() {
            return vec![];
        }
        read(&self.log).lines().map(String::from).collect()
    }
}

#[test]
fn deploy_starts_unit_from_the_new_release() {
    let c = case();
    c.deploy('1', "v1", 0).assert_ok();
    assert_eq!(c.runs(), ["1 v1"]);
    assert_eq!(systemctl(&["is-active", &c.unit]), "active");

    c.deploy('2', "v1", 0).assert_ok();
    assert_eq!(c.runs(), ["1 v1", "2 v1"]);
}

#[test]
fn changed_unit_file_takes_effect_after_reload() {
    let c = case();
    c.deploy('1', "v1", 0).assert_ok();
    c.deploy('2', "v2", 0).assert_ok();
    assert_eq!(c.runs(), ["1 v1", "2 v2"]);
}

#[test]
fn failing_unit_rolls_back_and_previous_release_runs_again() {
    let c = case();
    c.deploy('1', "v1", 0).assert_ok();
    let good = c.proj.current_release().unwrap();

    c.deploy('2', "v1", 1).assert_rejected("Rollback successful");

    assert_eq!(c.proj.current_release().unwrap(), good);
    // New release ran and failed, then the old one was restarted.
    assert_eq!(c.runs(), ["1 v1", "2 v1", "1 v1"]);
    assert_eq!(systemctl(&["is-active", &c.unit]), "active");
}
