//! The real agent binary and its real zbus `SystemdClient`, talking to a
//! fake systemd on a private `dbus-daemon` (needs the `dbus-daemon` binary,
//! nothing else). Covers the D-Bus wire protocol, `JobRemoved` handling,
//! ordering and rollback without any real systemd involvement.

use std::time::Duration;

use integration_tests::fake_systemd::FakeSystemd;
use integration_tests::{Agent, Env, Project, sha};

const APP: &str = "#!/bin/sh\n";
const UNIT_V1: &str = "[Service]\nExecStart=/srv/app/current/bin/app\n";
const UNIT_V2: &str = "[Service]\nExecStart=/srv/app/current/bin/app --v2\n";

// Fields drop in order: the agent (which dumps its log on panic) must go
// before the temp dir holding that log.
struct Setup {
    agent: Agent,
    fake: FakeSystemd,
    proj: Project,
    env: Env,
}

fn setup(allowed: &[&str]) -> Setup {
    let env = Env::new();
    let proj = env.onboard("webapp", allowed, 5);
    let fake = FakeSystemd::start();
    fake.watch_current(proj.install_dir.join("current"));
    let agent = env.start_agent_with(&[("DBUS_SYSTEM_BUS_ADDRESS", fake.address())]);
    Setup { agent, fake, proj, env }
}

impl Setup {
    fn deploy(&self, digit: char, units: &[(&str, &str)]) -> integration_tests::SendResult {
        let mut s = self.env.staging().bin("app", APP, 0o755);
        for (name, body) in units {
            s = s.unit(name, body);
        }
        let bundle = s.bundle("webapp", &sha(digit));
        self.env.send(&self.agent, &self.proj, &bundle)
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn stops_all_then_reloads_then_starts_all() {
    let s = setup(&["a.service", "b.service"]);
    s.deploy('1', &[("a.service", UNIT_V1), ("b.service", UNIT_V1)]).assert_ok();

    // Unit order within a phase follows the manifest; the phases must not mix.
    let calls = s.fake.calls();
    let mut phases: Vec<&str> = calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
    phases.dedup();
    assert_eq!(phases, ["stop", "reload", "start"], "{calls:?}");
    for u in ["a.service", "b.service"] {
        assert!(calls.contains(&format!("stop {u}")) && calls.contains(&format!("start {u}")));
    }
    // Units are started only after `current` points at the new release.
    let current = s.proj.current_release().unwrap();
    assert_eq!(s.fake.started_on(), [current.clone(), current]);
    assert_eq!(
        integration_tests::read(s.env.config_root.join("units/webapp/a.service")),
        UNIT_V1
    );
}

#[test]
fn unchanged_unit_files_skip_reload() {
    let s = setup(&["a.service"]);
    s.deploy('1', &[("a.service", UNIT_V1)]).assert_ok();
    s.fake.clear_calls();

    s.deploy('2', &[("a.service", UNIT_V1)]).assert_ok();
    assert_eq!(s.fake.calls(), strs(&["stop a.service", "start a.service"]));

    s.fake.clear_calls();
    s.deploy('3', &[("a.service", UNIT_V2)]).assert_ok();
    assert_eq!(s.fake.calls(), strs(&["stop a.service", "reload", "start a.service"]));
}

#[test]
fn bundle_without_units_makes_no_dbus_calls() {
    let s = setup(&["a.service"]);
    s.deploy('1', &[]).assert_ok();
    assert!(s.fake.calls().is_empty(), "{:?}", s.fake.calls());
}

#[test]
fn failed_start_job_rolls_back_and_restarts_all_allowed_units() {
    let s = setup(&["a.service", "b.service"]);
    s.deploy('1', &[("a.service", UNIT_V1)]).assert_ok();
    let previous = s.proj.current_release().unwrap();
    s.fake.clear_calls();
    s.fake.fail_job("start", "a.service", "failed");

    s.deploy('2', &[("a.service", UNIT_V1)])
        .assert_rejected("job for unit 'a.service' (start) failed with result status: 'failed'")
        .assert_rejected("Rollback successful");

    assert_eq!(s.proj.current_release().unwrap(), previous);
    // Rollback restarts every allowed unit, in project-config order.
    assert_eq!(
        s.fake.calls(),
        strs(&["stop a.service", "start a.service", "restart a.service", "restart b.service"])
    );
}

#[test]
fn failed_stop_job_rolls_back_before_starting_anything() {
    let s = setup(&["a.service"]);
    s.deploy('1', &[("a.service", UNIT_V1)]).assert_ok();
    let previous = s.proj.current_release().unwrap();
    s.fake.clear_calls();
    s.fake.fail_job("stop", "a.service", "timeout");

    s.deploy('2', &[("a.service", UNIT_V1)]).assert_rejected("failed to stop");

    assert_eq!(s.proj.current_release().unwrap(), previous);
    assert_eq!(s.fake.calls(), strs(&["stop a.service", "restart a.service"]));
}

#[test]
fn denied_reload_rolls_back() {
    // What a missing polkit rule for daemon-reload looks like on the wire.
    let s = setup(&["a.service"]);
    s.deploy('1', &[("a.service", UNIT_V1)]).assert_ok();
    let previous = s.proj.current_release().unwrap();
    s.fake.clear_calls();
    s.fake.deny("reload");

    s.deploy('2', &[("a.service", UNIT_V2)]).assert_rejected("daemon-reload failed");

    assert_eq!(s.proj.current_release().unwrap(), previous);
    assert_eq!(s.fake.calls(), strs(&["stop a.service", "reload", "restart a.service"]));
}

#[test]
fn failed_rollback_is_reported() {
    let s = setup(&["a.service"]);
    s.deploy('1', &[("a.service", UNIT_V1)]).assert_ok();
    s.fake.fail_job("start", "a.service", "failed");
    s.fake.fail_job("restart", "a.service", "failed");

    s.deploy('2', &[("a.service", UNIT_V1)]).assert_rejected("ROLLBACK FAILED");
}

#[test]
fn self_update_reloads_but_never_stops_or_starts_itself() {
    let env = Env::new();
    let proj = env.onboard("deploy-agent", &["deploy-agent.service", "helper.service"], 5);
    let fake = FakeSystemd::start();
    let mut agent = env.start_agent_with(&[("DBUS_SYSTEM_BUS_ADDRESS", fake.address())]);

    let bundle = env
        .staging()
        .bin("deploy-agent", APP, 0o755)
        .unit("deploy-agent.service", UNIT_V1)
        .unit("helper.service", UNIT_V1)
        .bundle("deploy-agent", &sha('1'));
    let res = env.send(&agent, &proj, &bundle);
    res.assert_ok();
    assert!(res.output.contains("200"), "{}", res.output);

    assert_eq!(fake.calls(), strs(&["stop helper.service", "reload", "start helper.service"]));
    assert!(agent.wait_exit(Duration::from_secs(5)).success());
}
