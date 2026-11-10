//! End-to-end: deploy-admin → deploy-ci bundle → deploy-ci send (TLS) →
//! deploy-agent, with NO D-Bus reachable at all. Anything that would need
//! systemd fails at connect time, so these can never touch the host.

use std::fs;
use std::time::Duration;

use integration_tests::{Env, mode, read, sha};

const APP: &str = "#!/bin/sh\necho hello\n";

#[test]
fn onboard_bundle_send_installs_release() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    assert_eq!(mode(env.config_root.join("secrets/webapp.key")), 0o600);
    assert!(proj.install_dir.join("shared").is_dir());

    let agent = env.start_agent();
    let bundle = env
        .staging()
        .bin("app", APP, 0o755)
        .bin("helper", "secret-ish", 0o700)
        .config("app.toml", "port = 8080\n")
        .config("nested/extra.json", "{}")
        .bundle("webapp", &sha('a'));

    let res = env.send(&agent, &proj, &bundle);
    res.assert_ok();
    assert!(res.output.contains("200"), "{}", res.output);

    let current = proj.current().expect("current symlink");
    let release = proj.current_release().unwrap();
    assert!(release.ends_with(&format!("_{}", &sha('a')[..12])), "{release}");
    assert_eq!(proj.releases(), std::slice::from_ref(&release));
    assert_eq!(read(current.join("bin/app")), APP);
    assert_eq!(mode(current.join("bin/app")), 0o755);
    assert_eq!(mode(current.join("bin/helper")), 0o700);
    assert_eq!(read(current.join("config/app.toml")), "port = 8080\n");
    assert_eq!(read(current.join("config/nested/extra.json")), "{}");
    let manifest = read(current.join("manifest.toml"));
    assert!(manifest.contains("project = \"webapp\""), "{manifest}");
    assert!(manifest.contains("source = \"integration-tests\""), "{manifest}");
    // The deployed binary actually runs from the new release.
    let out = std::process::Command::new(current.join("bin/app")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
}

#[test]
fn redeploys_swap_current_and_prune_to_retain_count() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 2);
    let agent = env.start_agent();

    for (i, d) in ['1', '2', '3'].into_iter().enumerate() {
        let bundle = env
            .staging()
            .bin("app", &format!("#!/bin/sh\necho v{i}\n"), 0o755)
            .bundle("webapp", &sha(d));
        env.send(&agent, &proj, &bundle).assert_ok();
        assert!(proj.current_release().unwrap().ends_with(&sha(d)[..12]));
    }

    let releases = proj.releases();
    assert_eq!(releases.len(), 2, "{releases:?}");
    assert!(releases.iter().all(|r| !r.ends_with(&sha('1')[..12])), "{releases:?}");
    assert_eq!(read(proj.current().unwrap().join("bin/app")), "#!/bin/sh\necho v2\n");
}

#[test]
fn shared_dir_survives_redeploys() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 1);
    let agent = env.start_agent();
    fs::write(proj.install_dir.join("shared/.env"), "DB=x\n").unwrap();

    for d in ['1', '2'] {
        let bundle = env.staging().bin("app", APP, 0o755).bundle("webapp", &sha(d));
        env.send(&agent, &proj, &bundle).assert_ok();
    }
    assert_eq!(read(proj.install_dir.join("shared/.env")), "DB=x\n");
}

// ---------------------------------------------------------------------------
// Rejections: nothing on disk may change
// ---------------------------------------------------------------------------

fn assert_untouched(env: &Env, proj: &integration_tests::Project) {
    assert_eq!(proj.current(), None);
    assert!(proj.releases().is_empty(), "{:?}", proj.releases());
    assert!(!env.config_root.join("units").exists());
}

#[test]
fn rejects_wrong_secret() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    let agent = env.start_agent();
    let bundle = env.staging().bin("app", APP, 0o755).bundle("webapp", &sha('a'));

    env.send_with(&agent, "webapp", "not-the-secret", &bundle, true)
        .assert_rejected("400")
        .assert_rejected("signature verification failed");
    assert_untouched(&env, &proj);
}

#[test]
fn rejects_secret_of_another_project() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    let other = env.onboard("other", &[], 5);
    let agent = env.start_agent();
    let bundle = env.staging().bin("app", APP, 0o755).bundle("webapp", &sha('a'));

    env.send_with(&agent, "webapp", &other.secret, &bundle, true)
        .assert_rejected("signature verification failed");
    assert_untouched(&env, &proj);
}

#[test]
fn rejects_unknown_project() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    let agent = env.start_agent();
    let bundle = env.staging().bin("app", APP, 0o755).bundle("ghost", &sha('a'));

    env.send_with(&agent, "ghost", &proj.secret, &bundle, true)
        .assert_rejected("Project ghost is not configured");
}

#[test]
fn rejects_bundle_built_for_another_project() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    let agent = env.start_agent();
    // Correctly signed for "webapp", but the manifest says "other".
    let bundle = env.staging().bin("app", APP, 0o755).bundle("other", &sha('a'));

    env.send(&agent, &proj, &bundle).assert_rejected("manifest project mismatch");
    assert_untouched(&env, &proj);
}

#[test]
fn rejects_unit_not_on_allow_list() {
    let env = Env::new();
    let proj = env.onboard("webapp", &["webapp.service"], 5);
    let agent = env.start_agent();
    let bundle = env
        .staging()
        .bin("app", APP, 0o755)
        .unit("sshd.service", "[Service]\nExecStart=/bin/true\n")
        .bundle("webapp", &sha('a'));

    env.send(&agent, &proj, &bundle)
        .assert_rejected("manifest unit sshd.service was not declared");
    assert_untouched(&env, &proj);
}

#[test]
fn send_without_ca_fails_tls_verification() {
    let env = Env::new();
    let proj = env.onboard("webapp", &[], 5);
    let agent = env.start_agent();
    let bundle = env.staging().bin("app", APP, 0o755).bundle("webapp", &sha('a'));

    let res = env.send_with(&agent, "webapp", &proj.secret, &bundle, false);
    assert!(!res.ok, "{}", res.output);
    assert!(!res.output.contains("400"), "must fail before HTTP: {}", res.output);
    assert_untouched(&env, &proj);
}

// ---------------------------------------------------------------------------
// Systemd needed but unreachable
// ---------------------------------------------------------------------------

#[test]
fn units_without_reachable_bus_roll_back_to_previous_release() {
    let env = Env::new();
    let proj = env.onboard("webapp", &["webapp.service"], 5);
    let agent = env.start_agent();

    // A unit-less deploy doesn't need systemd at all.
    let first = env.staging().bin("app", APP, 0o755).bundle("webapp", &sha('1'));
    env.send(&agent, &proj, &first).assert_ok();
    let previous = proj.current_release().unwrap();

    let second = env
        .staging()
        .bin("app", APP, 0o755)
        .unit("webapp.service", "[Service]\nExecStart=/bin/true\n")
        .bundle("webapp", &sha('2'));
    env.send(&agent, &proj, &second).assert_rejected("failed to connect to system D-Bus");

    assert_eq!(proj.current_release().unwrap(), previous);
    // The failed release stays on disk for inspection; a later deploy prunes it.
    assert_eq!(proj.releases().len(), 2);
}

// ---------------------------------------------------------------------------
// Self-update
// ---------------------------------------------------------------------------

#[test]
fn self_update_exits_agent_and_restart_prunes_old_releases() {
    const UNIT: &str = "deploy-agent.service";
    const UNIT_BODY: &str = "[Service]\nExecStart=/opt/deploy-agent/current/bin/deploy-agent\n";

    let env = Env::new();
    let proj = env.onboard("deploy-agent", &[UNIT], 1);
    // Pretend the unit file was installed by an earlier deploy, so these
    // bundles don't change it and therefore need no daemon-reload.
    let units_dir = env.config_root.join("units/deploy-agent");
    fs::create_dir_all(&units_dir).unwrap();
    fs::write(units_dir.join(UNIT), UNIT_BODY).unwrap();

    let mut releases_seen = Vec::new();
    for d in ['1', '2'] {
        let mut agent = env.start_agent();
        let bundle = env
            .staging()
            .bin("deploy-agent", "#!/bin/sh\n", 0o755)
            .unit(UNIT, UNIT_BODY)
            .bundle("deploy-agent", &sha(d));
        env.send(&agent, &proj, &bundle).assert_ok();
        // Replies first, then exits so systemd's Restart= brings it back.
        let status = agent.wait_exit(Duration::from_secs(5));
        assert!(status.success(), "{status}\n{}", agent.log());
        releases_seen.push(proj.current_release().unwrap());
    }

    // Self-updates skip pruning, so both releases are still there...
    assert_eq!(proj.releases(), releases_seen);
    // ...until the "restarted" agent prunes on startup.
    let agent = env.start_agent();
    assert_eq!(proj.releases(), [releases_seen[1].clone()], "{}", agent.log());
    assert_eq!(proj.current_release().unwrap(), releases_seen[1]);
}

#[test]
fn sigterm_shuts_agent_down_cleanly() {
    let env = Env::new();
    let mut agent = env.start_agent();
    let status = agent.terminate(Duration::from_secs(5));
    assert!(status.success(), "{status}\n{}", agent.log());
    assert!(agent.log().contains("SIGTERM received"), "{}", agent.log());
}
