//! Harness for black-box tests of the deploy pipeline.
//!
//! Everything runs as the current (non-root) user inside a fresh temp dir:
//! `deploy-admin onboard` writes config/secrets there, `deploy-ci bundle`
//! and `deploy-ci send` build and POST real bundles, and a real
//! `deploy-agent` process serves TLS on a random localhost port with a
//! throwaway CA generated per test.
//!
//! The agent's view of "the system bus" is controlled through
//! `DBUS_SYSTEM_BUS_ADDRESS`, which zbus honours for `Connection::system()`:
//! - default: a path that doesn't exist, so the agent can never reach the
//!   host's real systemd, whatever the test does;
//! - [`fake_systemd::FakeSystemd`]: a private `dbus-daemon` with a fake
//!   `org.freedesktop.systemd1.Manager` on it;
//! - the `systemd-tests` feature: the user's own `systemd --user` bus.

pub mod fake_systemd;

use std::fs;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Binaries
// ---------------------------------------------------------------------------

/// Path to one of the workspace binaries, building them on first use.
///
/// Integration tests only get `CARGO_BIN_EXE_*` for their own package, so
/// this runs `cargo build` for the three binary crates into the same target
/// dir and profile the test binary itself was built with.
pub fn bin(name: &str) -> PathBuf {
    static PROFILE_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = PROFILE_DIR.get_or_init(|| {
        // <target>/<profile>/deps/<test-binary>
        let exe = std::env::current_exe().expect("current_exe");
        let profile_dir = exe.parent().and_then(Path::parent).expect("target/<profile>").to_path_buf();
        let target_dir = profile_dir.parent().expect("target dir");
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

        // All three in one invocation, the way `cargo build --workspace` does
        // it: that unifies dependency features (ureq's rustls/ring next to
        // axum-server's aws-lc-rs), which once broke deploy-agent at startup.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.arg("build")
            .arg("--manifest-path")
            .arg(workspace.join("Cargo.toml"))
            .args(["--bins", "-p", "deploy-agent", "-p", "deploy-ci", "-p", "deploy-admin"])
            .env("CARGO_TARGET_DIR", target_dir);
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => {}
            Some("release") => {
                cmd.arg("--release");
            }
            Some(other) => {
                cmd.args(["--profile", other]);
            }
            None => panic!("unexpected test binary location {}", exe.display()),
        }
        let status = cmd.status().expect("running cargo build");
        assert!(status.success(), "building workspace binaries failed");
        profile_dir
    });
    let path = dir.join(name);
    assert!(path.is_file(), "binary {} not found", path.display());
    path
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap_or_else(|e| panic!("spawning {cmd:?}: {e}"))
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = run(cmd);
    assert!(
        out.status.success(),
        "{cmd:?} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// ---------------------------------------------------------------------------
// Environment: temp dirs, certs, onboarding
// ---------------------------------------------------------------------------

/// One isolated "host": config root, install dirs, TLS material.
pub struct Env {
    pub tmp: TempDir,
    pub config_root: PathBuf,
    pub ca_cert: PathBuf,
    server_cert: PathBuf,
    server_key: PathBuf,
    counter: AtomicUsize,
}

/// A project onboarded through `deploy-admin`.
#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub install_dir: PathBuf,
    /// Contents of the secret file, as `deploy-ci send` expects it in env.
    pub secret: String,
}

impl Project {
    pub fn current(&self) -> Option<PathBuf> {
        fs::read_link(self.install_dir.join("current")).ok().map(|t| {
            if t.is_absolute() { t } else { self.install_dir.join(t) }
        })
    }

    /// Name of the release `current` points at.
    pub fn current_release(&self) -> Option<String> {
        self.current().map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
    }

    /// Release directory names, sorted.
    pub fn releases(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(self.install_dir.join("releases")) else { return vec![] };
        let mut v: Vec<String> =
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    pub fn new() -> Self {
        let tmp = tempfile::Builder::new().prefix("deploy-it-").tempdir().unwrap();
        let config_root = tmp.path().join("etc/deploy-agent");
        let tls = tmp.path().join("tls");
        fs::create_dir_all(&tls).unwrap();
        let (ca_cert, server_cert, server_key) = write_test_pki(&tls);
        Self { tmp, config_root, ca_cert, server_cert, server_key, counter: AtomicUsize::new(0) }
    }

    pub fn path(&self) -> &Path {
        self.tmp.path()
    }

    fn next(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// `deploy-admin onboard <name> --units ... --retain-count N`.
    pub fn onboard(&self, name: &str, units: &[&str], retain_count: usize) -> Project {
        let install_dir = self.path().join("srv").join(name);
        let mut cmd = Command::new(bin("deploy-admin"));
        cmd.arg("onboard")
            .arg(name)
            .arg("--config-root")
            .arg(&self.config_root)
            .arg("--install-dir")
            .arg(&install_dir)
            .arg("--retain-count")
            .arg(retain_count.to_string())
            // Chown to ourselves so the best-effort chown succeeds quietly.
            .arg("--agent-user")
            .arg(current_user())
            .arg("--agent-group")
            .arg(current_group());
        if !units.is_empty() {
            cmd.arg("--units").arg(units.join(","));
        }
        run_ok(&mut cmd);
        let secret =
            fs::read_to_string(self.config_root.join("secrets").join(format!("{name}.key"))).unwrap();
        Project { name: name.into(), install_dir, secret }
    }

    /// Starts a staging directory for `deploy-ci bundle`.
    pub fn staging(&self) -> Staging {
        let dir = self.path().join(format!("staging-{}", self.next()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        Staging { dir, out_dir: self.path().to_path_buf() }
    }

    /// Spawns `deploy-agent` with no reachable D-Bus.
    pub fn start_agent(&self) -> Agent {
        let dead_bus = format!("unix:path={}", self.path().join("no-such-bus").display());
        self.start_agent_with(&[("DBUS_SYSTEM_BUS_ADDRESS", &dead_bus)])
    }

    /// Spawns `deploy-agent`; `env` is applied last and can override anything.
    pub fn start_agent_with(&self, env: &[(&str, &str)]) -> Agent {
        let port = free_port();
        let log = self.path().join(format!("agent-{}.log", self.next()));
        let log_file = fs::File::create(&log).unwrap();
        let mut cmd = Command::new(bin("deploy-agent"));
        cmd.env_remove("DEPLOY_AGENT_UNIT_NAME")
            .env_remove("DEPLOY_AGENT_PROJECT_NAME")
            .env("RUST_LOG", "info")
            .env("DEPLOY_AGENT_CONFIG_ROOT", &self.config_root)
            .env("DEPLOY_AGENT_TLS_CERT", &self.server_cert)
            .env("DEPLOY_AGENT_TLS_KEY", &self.server_key)
            .env("DEPLOY_AGENT_BIND_ADDR", format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(log_file.try_clone().unwrap())
            .stderr(log_file);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawning deploy-agent");
        let mut agent = Agent { child, port, log, exited: None };
        agent.wait_ready();
        agent
    }

    /// `deploy-ci send` with the project's own secret and our CA.
    pub fn send(&self, agent: &Agent, project: &Project, bundle: &Path) -> SendResult {
        self.send_with(agent, &project.name, &project.secret, bundle, true)
    }

    pub fn send_with(
        &self,
        agent: &Agent,
        project: &str,
        secret: &str,
        bundle: &Path,
        trust_ca: bool,
    ) -> SendResult {
        let mut cmd = Command::new(bin("deploy-ci"));
        cmd.arg("send")
            .arg("--bundle")
            .arg(bundle)
            .arg("--project")
            .arg(project)
            .arg("--url")
            .arg(agent.url())
            .env("DEPLOY_HMAC_KEY", secret);
        if trust_ca {
            cmd.arg("--ca-cert").arg(&self.ca_cert);
        }
        let out = run(&mut cmd);
        SendResult {
            ok: out.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }
}

#[derive(Debug)]
pub struct SendResult {
    pub ok: bool,
    /// stdout + stderr of `deploy-ci send`; includes the agent's response.
    pub output: String,
}

impl SendResult {
    #[track_caller]
    pub fn assert_ok(&self) -> &Self {
        assert!(self.ok, "deploy should have succeeded:\n{}", self.output);
        self
    }

    #[track_caller]
    pub fn assert_rejected(&self, needle: &str) -> &Self {
        assert!(!self.ok, "deploy should have failed:\n{}", self.output);
        assert!(self.output.contains(needle), "expected {needle:?} in:\n{}", self.output);
        self
    }
}

// ---------------------------------------------------------------------------
// Staging / bundling
// ---------------------------------------------------------------------------

pub struct Staging {
    dir: PathBuf,
    out_dir: PathBuf,
}

impl Staging {
    pub fn bin(self, name: &str, contents: &str, mode: u32) -> Self {
        let p = self.dir.join("bin").join(name);
        fs::write(&p, contents).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        self
    }

    pub fn unit(self, name: &str, contents: &str) -> Self {
        fs::create_dir_all(self.dir.join("systemd")).unwrap();
        fs::write(self.dir.join("systemd").join(name), contents).unwrap();
        self
    }

    pub fn config(self, rel: &str, contents: &str) -> Self {
        let p = self.dir.join("config").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
        self
    }

    /// `deploy-ci bundle`; returns the .tar.zst path.
    pub fn bundle(self, project: &str, git_sha: &str) -> PathBuf {
        let out = self.out_dir.join(format!(
            "{}.tar.zst",
            self.dir.file_name().unwrap().to_string_lossy()
        ));
        run_ok(
            Command::new(bin("deploy-ci"))
                .arg("bundle")
                .arg("--staging-dir")
                .arg(&self.dir)
                .arg("--project")
                .arg(project)
                .arg("--git-sha")
                .arg(git_sha)
                .arg("--out")
                .arg(&out)
                .args(["--meta", "source=integration-tests"]),
        );
        out
    }
}

/// A 40-char fake git sha made of one repeated hex digit. Release dirs sort
/// by `<timestamp>_<sha12>`, so within one second use increasing digits.
pub fn sha(digit: char) -> String {
    std::iter::repeat_n(digit, 40).collect()
}

// ---------------------------------------------------------------------------
// The agent process
// ---------------------------------------------------------------------------

pub struct Agent {
    child: Child,
    port: u16,
    log: PathBuf,
    exited: Option<std::process::ExitStatus>,
}

impl Agent {
    pub fn url(&self) -> String {
        // An IP SAN is in the cert; avoids localhost resolving to ::1.
        format!("https://127.0.0.1:{}/deploy", self.port)
    }

    pub fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("deploy-agent exited during startup ({status}):\n{}", self.log());
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "deploy-agent never listened:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Sends SIGTERM and waits for the process to exit.
    pub fn terminate(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
        self.wait_exit(timeout)
    }

    /// Waits for the process to exit on its own (self-update path).
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.exited = Some(status);
                return status;
            }
            assert!(Instant::now() < deadline, "deploy-agent did not exit:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self.exited.is_none() {
            // SIGTERM first so the agent exits normally (and, under
            // cargo llvm-cov, writes its coverage profile); SIGKILL if it hangs.
            let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if std::thread::panicking() {
            eprintln!("---- deploy-agent log ({}) ----\n{}", self.log.display(), self.log());
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn write_test_pki(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "deploy-agent integration test CA");
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let server_params =
        CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()]).unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let paths = (dir.join("ca.pem"), dir.join("cert.pem"), dir.join("key.pem"));
    fs::write(&paths.0, ca_cert.pem()).unwrap();
    fs::write(&paths.1, server_cert.pem()).unwrap();
    fs::write(&paths.2, server_key.serialize_pem()).unwrap();
    (paths.0, paths.1, paths.2)
}

fn id(flag: &str) -> String {
    let out = run_ok(Command::new("id").arg(flag));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn current_user() -> String {
    id("-un")
}

fn current_group() -> String {
    id("-gn")
}

/// Reads a whole file, panicking with the path on error.
pub fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let mut s = String::new();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    s
}

pub fn mode(path: impl AsRef<Path>) -> u32 {
    fs::metadata(path.as_ref()).unwrap().permissions().mode() & 0o777
}
