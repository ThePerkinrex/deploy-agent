# deploy-agent

A small, config-driven deployment agent for a Raspberry Pi (or any systemd
host). GitHub Actions builds your binaries, packs them into a signed
bundle, and sends it over Tailscale to `deploy-agent`. The agent verifies
the bundle, unpacks it into a new release directory, swaps a `current`
symlink, and restarts your systemd units over D-Bus. Nothing is compiled
on the Pi, the agent never runs as root, and it updates itself through
the same pipeline.

```
GitHub Actions                                  Raspberry Pi
┌───────────────────────────┐   HTTPS over     ┌───────────────────────────────┐
│ build → stage bin/,       │   Tailscale      │ deploy-agent (non-root)       │
│ systemd/, config/         │  ─────────────▶  │  verify HMAC + checksums      │
│ deploy-ci bundle + send   │  POST /deploy    │  releases/<ts>_<sha>/         │
│ (HMAC-signed tar.zst)     │                  │  swap current → stop/start    │
└───────────────────────────┘                  │  units via D-Bus + polkit     │
                                               └───────────────────────────────┘
```

## Workspace

| Crate | Runs on | Purpose |
|---|---|---|
| [`deploy-common`](deploy-common/) | — (library) | Manifest schema, bundle build/extract, release layout, HMAC signing string, project config. Shared by everything else. |
| [`deploy-agent`](deploy-agent/) | Pi, as a systemd service | HTTPS server (axum + rustls) exposing `POST /deploy`; talks to systemd via `zbus`. |
| [`deploy-ci`](deploy-ci/) | CI runner (x86_64) | `bundle` builds the manifest + `.tar.zst`; `send` signs and POSTs it. |
| [`deploy-admin`](deploy-admin/) | Pi, by hand, under `sudo` | `onboard` creates a project's secret, config and directories. |

Other pieces:

- `deploy-agent/deploy-agent.service`: the agent's own systemd unit.
- `deploy-agent/deploy-agent-generator.sh` + `install-generator.sh`: a
  root-owned systemd generator. It symlinks agent-written unit files from
  `/etc/deploy-agent/units/` into systemd's search path.
- `deploy-agent/polkit/49-deploy-agent.rules`: lets the `deploy-agent` user
  stop, start and restart only allow-listed units, and run daemon-reload.
- `create_certs.sh` / `renew_server_cert.sh`: create a local CA and the
  agent's TLS server certificate.
- `.github/workflows/`: the reusable `deploy.yml`, `self-deploy.yml` and
  `release-deploy-ci.yml`.

## How a deploy works

1. **CI** stages `bin/` (required, flat), `systemd/` (optional, flat) and
   `config/` (optional). It runs `deploy-ci bundle`, which writes
   `manifest.toml` with a sha256 for every binary and unit file, then
   `deploy-ci send`.
2. The request carries `X-Deploy-Project`, `X-Deploy-Timestamp` (unix
   seconds) and `X-Deploy-Signature: sha256=<hex>`. The signature is
   HMAC-SHA256 over `"<timestamp>\n<project>\n<sha256(body)>"`, using that
   project's secret.
3. **The agent** rejects the request if:
   - the timestamp is more than 5 minutes off,
   - the project has no config,
   - the signature doesn't match,
   - an archive entry is unsafe (absolute path, `..`, symlink or hardlink),
   - `manifest.project` differs from the header,
   - any manifest unit is missing from the project's `allowed_units`,
   - or a checksum doesn't match.
4. The release moves to `<install_dir>/releases/<YYYY-MM-DDTHH-MM-SSZ>_<sha12>/`.
   Unit files are copied to `/etc/deploy-agent/units/<project>/`, and
   `current` is swapped atomically.
5. All units are **stopped**, then `daemon-reload` runs if any unit file
   changed, then all units are **started**. Each step waits for systemd's
   `JobRemoved` signal. If any step fails, the agent swaps `current` back
   and restarts the units the deploy stopped (never other allow-listed
   units, and never the agent's own unit). Deploys of the same project
   are serialised; different projects can deploy concurrently.
6. Releases beyond `retain_count` are pruned. The release `current` points
   to is never pruned.

The response is `200 deployed release <id>` on success. Any failure
returns `400 deploy failed: <reason>`; details go to the journal.

**Self-update:** if the manifest contains `deploy-agent.service` (see
`DEPLOY_AGENT_UNIT_NAME`), the agent doesn't restart itself over D-Bus.
It replies, exits about 500 ms later, and systemd's `Restart=always`
starts the new binary through the `current` symlink. Old agent releases
are pruned at startup.

## On-disk layout (Pi)

```
/etc/deploy-agent/
  projects/<project>.toml     # per-project policy (written by deploy-admin)
  secrets/<project>.key       # HMAC secret, mode 600
  units/<project>/*.service   # written by the agent, read by the generator
  tls/{cert,key}.pem
/srv/apps/<project>/          # or /opt/deploy-agent for the agent itself
  releases/<ts>_<sha>/{bin,systemd,config,manifest.toml}
  current -> releases/<ts>_<sha>
  shared/                     # persists across releases (e.g. .env)
```

Project config (`projects/<project>.toml`):

```toml
install_dir = "/srv/apps/myproj"
allowed_units = ["myproj-api.service", "myproj-worker.service"]
retain_count = 5                       # default 5
secret_path = "/etc/deploy-agent/secrets/myproj.key"

[health_check]                         # optional; parsed but not used yet
url = "http://127.0.0.1:9000/healthz"
timeout_secs = 10
```

Your units should use `ExecStart=<install_dir>/current/bin/<name>` and keep
runtime secrets in `shared/` (e.g. `EnvironmentFile=<install_dir>/shared/.env`).

## Configuration

`deploy-agent` reads these environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `DEPLOY_AGENT_CONFIG_ROOT` | `/etc/deploy-agent` | Root holding `projects/`, `units/`. |
| `DEPLOY_AGENT_BIND_ADDR` | `127.0.0.1:8443` | Listen address; set to the Pi's Tailscale IP in production. |
| `DEPLOY_AGENT_TLS_CERT` | `/tmp/deploy-agent-test/cert.pem` | Server certificate (PEM). TLS is mandatory. |
| `DEPLOY_AGENT_TLS_KEY` | `/tmp/deploy-agent-test/key.pem` | Server private key (PEM). |
| `DEPLOY_AGENT_UNIT_NAME` | `deploy-agent.service` | Unit name that marks a deploy as a self-update. |
| `DEPLOY_AGENT_PROJECT_NAME` | `deploy-agent` | Project used for startup self-pruning. |
| `RUST_LOG` | — | `tracing` filter, e.g. `info`. |

The request body limit is 200 MB.

## Building

```bash
cargo build --workspace
cargo test --workspace

# For the Pi (no cross/Docker; linker set in .cargo/config.toml):
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu -p deploy-agent -p deploy-admin
```

`cargo test --workspace` also runs [`integration-tests`](integration-tests/).
These drive the real binaries end to end and never touch systemd: there's
either no bus at all, or a fake systemd on a private `dbus-daemon`. A third
tier runs against your *user* systemd manager. It is opt-in, via
`cargo test -p integration-tests --features systemd-tests`. See its README for
why that tier is safe to run on a dev machine.

`deploy-common/examples/` holds `bundle_roundtrip` (build, extract,
verify and swap in temp dirs) and `sign_bundle` (build and sign a test
bundle by hand). Run them with `cargo run -p deploy-common --example <name>`.

## Usage

### Onboard a project (on the Pi)

```bash
sudo deploy-admin onboard myproj \
  --units myproj-api.service,myproj-worker.service \
  [--install-dir /srv/apps/myproj] [--retain-count 5] \
  [--health-check-url URL] [--health-check-timeout-secs 10] \
  [--config-root /etc/deploy-agent] [--agent-user deploy-agent] [--agent-group deploy-agent] \
  [--force]   # regenerate an existing secret
```

This command can be re-run safely. It prints the lines to add to the
polkit rule's `allowedUnits` by hand.

### Deploy from a downstream repo

Stage your build, upload it as an artifact, and call the reusable workflow:

```yaml
jobs:
  build:
    # ... cross-compile, then:
    # mkdir -p staging/bin staging/systemd; cp ... ; upload-artifact name: myproj-staging
  deploy:
    needs: build
    uses: ThePerkinrex/deploy-agent/.github/workflows/deploy.yml@v1.0.0
    with:
      project: myproj
      artifact-name: myproj-staging
      staging-dir: staging
      agent-host: ${{ vars.AGENT_HOST }}
      port: "8443"
      deploy-ci-repo: ThePerkinrex/deploy-agent
    secrets:
      hmac-key: ${{ secrets.HMAC_KEY }}
      tailscale-oauth-client-id: ${{ secrets.TS_OAUTH_CLIENT_ID }}
      tailscale-oauth-client-secret: ${{ secrets.TS_OAUTH_CLIENT_SECRET }}
      deploy-ca-cert: ${{ secrets.DEPLOY_CA_CERT }}
```

### Use `deploy-ci` directly

```bash
deploy-ci bundle --staging-dir staging --project myproj --git-sha "$(git rev-parse HEAD)" \
  --out bundle.tar.zst [--meta branch=main ...]
DEPLOY_HMAC_KEY=... deploy-ci send --bundle bundle.tar.zst --project myproj \
  --url https://rpi.example.ts.net:8443/deploy --ca-cert certs/ca-cert.pem \
  [--hmac-key-env DEPLOY_HMAC_KEY]
```

Without `--ca-cert`, `send` uses the system root store, so it fails
against the self-signed agent certificate.

## Releasing `deploy-ci`

Pushing a `deploy-ci-v*` tag runs `release-deploy-ci.yml`. It attaches
`deploy-ci-x86_64-unknown-linux-gnu` to a GitHub Release, which
`deploy.yml` downloads (`deploy-ci-source: download`, `deploy-ci-version:
latest` or a tag).

## Status and limitations

What works: HMAC auth, safe extraction, atomic swap, stop/reload/start
with rollback, self-update, onboarding, and the CI workflows.

Not done yet:

- Health checks (`health_check` is parsed but never used).
- `/status` endpoint.
- Body-size hardening.
- Rollback doesn't restore the previous unit files.
- `BinaryEntry.mode` isn't applied.
- The generator ignores `DEPLOY_AGENT_CONFIG_ROOT`.
- `deploy.yml`'s default `deploy-ci-repo` names
  `ThePerkinrex/deploy-agent-workspace`, but this repository is
  `ThePerkinrex/deploy-agent`. Pass the input explicitly for now.

## Documentation

- [`docs/00-consolidated-design.md`](docs/00-consolidated-design.md): design decisions and current status (the reference doc).
- [`docs/05-pi-install-and-project-onboarding-guide.md`](docs/05-pi-install-and-project-onboarding-guide.md): runbook for bootstrapping the Pi, onboarding projects, and setting up CI.
- `docs/01`–`04`, `docs/rpi-deploy-agent-handoff.md`: historical step-by-step build notes. They are kept for the reasoning behind each decision, and each starts with a note on how the code differs.
