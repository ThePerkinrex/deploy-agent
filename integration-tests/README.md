# integration-tests

Black-box tests that drive the real `deploy-admin`, `deploy-ci` and
`deploy-agent` binaries: onboarding, then bundling, then a signed HTTPS send
(throwaway CA, random localhost port), then the agent's verify, unpack and
swap steps. Everything lives in a fresh temp dir and runs as your user. The
harness builds the binaries itself (one `cargo build` per package, see
`bin()` in `src/lib.rs`).

```sh
cargo test -p integration-tests                            # default tiers, no systemd
cargo test -p integration-tests --features systemd-tests   # adds the real-systemd tier
```

Coverage ([cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)). The
binaries the tests spawn are instrumented too, and the harness stops the agent
with SIGTERM so it writes its profile:

```sh
cargo llvm-cov --workspace --exclude integration-tests --ignore-filename-regex integration-tests/   # unit only
cargo llvm-cov -p integration-tests --features systemd-tests --ignore-filename-regex integration-tests/ # integration only
cargo llvm-cov --workspace --features integration-tests/systemd-tests --ignore-filename-regex integration-tests/ --html
```

## Tiers

| File | Needs | What answers D-Bus |
|---|---|---|
| `tests/deploy_flow.rs` | nothing | nothing. `DBUS_SYSTEM_BUS_ADDRESS` points at a path that doesn't exist, so the agent cannot reach the host's systemd whatever a test does. |
| `tests/fake_systemd.rs` | the `dbus-daemon` binary | a fake `org.freedesktop.systemd1.Manager` (`src/fake_systemd.rs`) on a private `dbus-daemon`. It records every call and lets a test choose each job's `JobRemoved` result. The agent's real zbus client is exercised. |
| `tests/real_systemd.rs` | `--features systemd-tests` and a user session | your **user** manager (`systemd --user`), never PID 1 |

## Why the systemd tier can't break the machine

- The agent's `DBUS_SYSTEM_BUS_ADDRESS` is set to the *session* bus. Every
  `StopUnit`/`StartUnit`/`Reload` goes to `systemd --user`. That needs no root
  and no polkit, and it can only run processes as you.
- Unit names are unique per run (`deploy-agent-it-<pid>-<n>-app.service`), so
  they can't collide with real units.
- Units are `Type=oneshot` shell scripts that append a line to a file in the
  test's temp dir. They don't open ports and nothing keeps running.
- The only thing written outside the temp dir is one symlink per unit in
  `$XDG_RUNTIME_DIR/systemd/user/`. It stands in for the generator. That
  directory is a tmpfs that is wiped at logout. A drop guard stops the units,
  removes the links, then runs `daemon-reload` and `reset-failed`, including
  when a test panics.

To check cleanup after a run: `systemctl --user list-units --all 'deploy-agent-it-*'`.

Not covered: the generator script and the polkit rules. Both are
system-level by nature. To test them, run the suite in a throwaway VM or
container that has systemd as PID 1 (e.g. `systemd-nspawn -b`, or podman with
`--systemd=always`), as a non-root `deploy-agent` user.

On GitHub Actions the runner has no user manager by default. Run
`sudo loginctl enable-linger $USER`, and export
`XDG_RUNTIME_DIR=/run/user/$(id -u)` before enabling the feature.
