//! A fake systemd on a private, throwaway `dbus-daemon`.
//!
//! The agent's real `SystemdClient` (zbus proxy, `JobRemoved` matching,
//! error mapping) talks to this instead of PID 1: point the agent's
//! `DBUS_SYSTEM_BUS_ADDRESS` at [`FakeSystemd::address`]. The fake owns
//! `org.freedesktop.systemd1`, implements the four Manager methods the agent
//! uses, records every call, and answers each job with a `JobRemoved` signal
//! whose result can be scripted per unit.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

#[derive(Default)]
struct State {
    calls: Vec<String>,
    /// (op, unit) -> JobRemoved result ("failed", "timeout", ...).
    job_results: HashMap<(String, String), String>,
    /// op -> D-Bus error returned by the method call itself.
    method_errors: HashMap<String, String>,
    /// If set, every `start` records which release this symlink points at.
    watch_current: Option<PathBuf>,
    started_on: Vec<String>,
}

struct Manager {
    state: Arc<Mutex<State>>,
    next_job: AtomicU32,
}

impl Manager {
    fn job(
        &self,
        emitter: SignalEmitter<'_>,
        op: &str,
        unit: String,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(format!("{op} {unit}"));
        if let Some(err) = st.method_errors.get(op) {
            return Err(zbus::fdo::Error::AccessDenied(err.clone()));
        }
        if op == "start"
            && let Some(link) = &st.watch_current
        {
            let target = fs::read_link(link)
                .map(|t| t.file_name().unwrap().to_string_lossy().into_owned())
                .unwrap_or_else(|_| "<none>".into());
            st.started_on.push(target);
        }
        let result = st
            .job_results
            .get(&(op.to_string(), unit.clone()))
            .cloned()
            .unwrap_or_else(|| "done".into());
        drop(st);

        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{id}")).unwrap();
        let reply = path.clone();
        let emitter = emitter.to_owned();
        // Emit after the method reply has gone out, like systemd does.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = Manager::job_removed(&emitter, id, path.as_ref(), &unit, &result).await;
        });
        Ok(reply)
    }
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    fn start_unit(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: String,
        _mode: String,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.job(emitter, "start", name)
    }

    fn stop_unit(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: String,
        _mode: String,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.job(emitter, "stop", name)
    }

    fn restart_unit(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: String,
        _mode: String,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.job(emitter, "restart", name)
    }

    fn reload(&self) -> zbus::fdo::Result<()> {
        let mut st = self.state.lock().unwrap();
        st.calls.push("reload".into());
        match st.method_errors.get("reload") {
            Some(err) => Err(zbus::fdo::Error::AccessDenied(err.clone())),
            None => Ok(()),
        }
    }

    #[zbus(signal)]
    async fn job_removed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        job: ObjectPath<'_>,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}

pub struct FakeSystemd {
    _dir: TempDir,
    daemon: Child,
    address: String,
    state: Arc<Mutex<State>>,
    // Field order matters: drop the connection before the runtime.
    _conn: zbus::Connection,
    _rt: tokio::runtime::Runtime,
}

impl FakeSystemd {
    /// Starts a private `dbus-daemon` and registers the fake Manager on it.
    /// Panics with an explanation if `dbus-daemon` isn't installed.
    pub fn start() -> Self {
        let dir = tempfile::Builder::new().prefix("deploy-it-bus-").tempdir().unwrap();
        let socket = dir.path().join("bus");
        let config = dir.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                socket.display()
            ),
        )
        .unwrap();

        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| {
                panic!("could not run `dbus-daemon` ({e}); install the dbus package to run these tests")
            });
        // The address is printed once the socket is listening.
        let mut line = String::new();
        BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let address = line.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no address");

        let state = Arc::new(Mutex::new(State::default()));
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let conn = rt.block_on(async {
            zbus::connection::Builder::address(address.as_str())?
                .name("org.freedesktop.systemd1")?
                .serve_at(
                    "/org/freedesktop/systemd1",
                    Manager { state: state.clone(), next_job: AtomicU32::new(1) },
                )?
                .build()
                .await
        })
        .expect("registering fake systemd on private bus");

        Self { _dir: dir, daemon, address, state, _conn: conn, _rt: rt }
    }

    /// Value for the agent's `DBUS_SYSTEM_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Every Manager call so far, e.g. `["stop a.service", "reload", ...]`.
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.state.lock().unwrap().calls.clear();
    }

    /// Makes the job for `op` (`start`/`stop`/`restart`) on `unit` finish
    /// with `result` instead of `"done"`.
    pub fn fail_job(&self, op: &str, unit: &str, result: &str) {
        self.state.lock().unwrap().job_results.insert((op.into(), unit.into()), result.into());
    }

    /// Makes the method call for `op` itself return a D-Bus AccessDenied
    /// error, the way a missing polkit rule would.
    pub fn deny(&self, op: &str) {
        self.state.lock().unwrap().method_errors.insert(op.into(), format!("fake: {op} denied"));
    }

    /// Record which release `current_link` points at whenever a unit starts.
    pub fn watch_current(&self, current_link: PathBuf) {
        self.state.lock().unwrap().watch_current = Some(current_link);
    }

    pub fn started_on(&self) -> Vec<String> {
        self.state.lock().unwrap().started_on.clone()
    }
}

impl Drop for FakeSystemd {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
