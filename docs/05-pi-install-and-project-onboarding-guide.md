# Pi Install, Project Onboarding & Downstream Workflow Guide

Practical, step-by-step guide to: bootstrap `deploy-agent` on a fresh Pi,
onboard a project (including the `deploy-agent` self-project), and wire up
a downstream repo's own GitHub Actions workflow to deploy through it. For
the *design* behind any of this, see `docs/00-consolidated-design.md`.

## Before you start: get the Pi's Tailscale IP

`deploy-agent`'s bind address is configurable via `DEPLOY_AGENT_BIND_ADDR`
(`host:port`), defaulting to `127.0.0.1:8443` for local dev. In
production you want it bound to the Tailscale interface specifically, not
`0.0.0.0` — join the Pi to the tailnet first, then read off its stable
address:

```bash
tailscale ip -4
```

That address doesn't change on reboot once assigned, so this is a
one-time lookup, not something the agent needs to discover dynamically at
startup. You'll set it as `DEPLOY_AGENT_BIND_ADDR=<that-ip>:8443` in
`deploy-agent.service` in step 1.4 below.

---

## Part 1 — Initial install on the Pi (one-time, manual, root)

All of this is deliberately out-of-band, by hand — this is the
"bootstrapping is out of band" step the design docs call out, and it's
what makes the automated pipeline trustworthy afterward (nothing here is
touched by routine deploys).

### 1.1 — System user

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin deploy-agent
```

This is the identity `deploy-agent` runs as, that the polkit rule
authorizes, and that `deploy-admin --agent-user`/`--agent-group` default to.

### 1.2 — Directory layout

| Path | Owner | Purpose |
|---|---|---|
| `/etc/deploy-agent/projects/` | root (files inside: `deploy-agent`) | per-project `ProjectConfig` TOML |
| `/etc/deploy-agent/secrets/` | root (files inside: `deploy-agent`, mode 600) | per-project HMAC secrets |
| `/etc/deploy-agent/units/` | **`deploy-agent`** (the whole tree) | agent-owned unit files, read by the generator |
| `/etc/deploy-agent/tls/` | `deploy-agent` | TLS cert/key the agent's rustls listener uses |
| `/opt/deploy-agent/{releases,shared}` + `current` symlink | `deploy-agent` | the self-project's own release tree |
| `/srv/apps/<project>/{releases,shared}` + `current` symlink | `deploy-agent` | one per onboarded project |

```bash
sudo mkdir -p /etc/deploy-agent/{projects,secrets,units,tls}
sudo chown deploy-agent:deploy-agent /etc/deploy-agent/units /etc/deploy-agent/tls
sudo chgrp deploy-agent /etc/deploy-agent/secrets
sudo chmod 750 /etc/deploy-agent/secrets
```

`projects/` and `secrets/` stay root-owned directories — `deploy-admin`
(run under `sudo`) writes into them and chowns the *files* it creates to
`deploy-agent`, so the agent can read its own config/secret without
owning the containing directory. `units/` and `tls/` are owned outright
by `deploy-agent` because the agent process itself writes into `units/`
at deploy time (`sync_unit_files` in `deploy-agent/src/systemd.rs`) and
needs to read the TLS files at startup.

### 1.3 — TLS material

From your own machine (not the Pi — keep the CA key offline):

```bash
# Edit TAILNET_DOMAIN and TAILSCALE_IP at the top of the script first.
./create_certs.sh
```

Produces `certs/{ca-cert,ca-key,server-cert,server-key}.pem`. Copy only
the server cert/key to the Pi:

```bash
scp certs/server-cert.pem certs/server-key.pem pi:/tmp/
ssh pi 'sudo mv /tmp/server-cert.pem /etc/deploy-agent/tls/cert.pem &&
         sudo mv /tmp/server-key.pem /etc/deploy-agent/tls/key.pem &&
         sudo chown deploy-agent:deploy-agent /etc/deploy-agent/tls/*.pem &&
         sudo chmod 600 /etc/deploy-agent/tls/key.pem'
```

Keep `ca-cert.pem` (public) around — it's what every `deploy-ci send
--ca-cert` call and every downstream repo's `DEPLOY_CA_CERT` secret needs.
Keep `ca-key.pem` (private) off the Pi and off CI entirely; it's only
needed to issue/renew the server cert.

### 1.4 — First binary + systemd unit (manual — this is the one deploy that can't deploy itself)

Cross-compile once, from a dev machine (no `cross`/Docker needed — see
`.cargo/config.toml`):

```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu   # or your distro's equivalent
cargo build --release --target aarch64-unknown-linux-gnu -p deploy-agent -p deploy-admin
```

Place the binary in the release-directory shape the agent expects (so
future real deploys' `current` symlink swap behaves identically to this
first one):

```bash
ssh pi 'sudo mkdir -p /opt/deploy-agent/releases/bootstrap/bin /opt/deploy-agent/shared'
scp target/aarch64-unknown-linux-gnu/release/deploy-agent pi:/tmp/
ssh pi 'sudo mv /tmp/deploy-agent /opt/deploy-agent/releases/bootstrap/bin/deploy-agent &&
         sudo chmod 755 /opt/deploy-agent/releases/bootstrap/bin/deploy-agent &&
         sudo ln -s releases/bootstrap /opt/deploy-agent/current &&
         sudo chown -R deploy-agent:deploy-agent /opt/deploy-agent'
```

Also copy `deploy-admin` somewhere on the Pi's `PATH` for later onboarding
commands — it is **never** part of the automated pipeline, so this manual
copy is the only way it ever gets there or gets updated:

```bash
scp target/aarch64-unknown-linux-gnu/release/deploy-admin pi:/tmp/
ssh pi 'sudo mv /tmp/deploy-admin /usr/local/sbin/deploy-admin && sudo chmod 755 /usr/local/sbin/deploy-admin'
```

Install the unit file (from this repo's `deploy-agent/deploy-agent.service`),
uncommenting the TLS and bind-address env lines — use the Tailscale IP you
looked up above, not `127.0.0.1`:

```bash
scp deploy-agent/deploy-agent.service pi:/tmp/
ssh pi <<'EOF'
sudo cp /tmp/deploy-agent.service /etc/systemd/system/deploy-agent.service
TS_IP="$(tailscale ip -4)"
sudo sed -i \
  -e 's|# Environment=DEPLOY_AGENT_TLS_CERT=.*|Environment=DEPLOY_AGENT_TLS_CERT=/etc/deploy-agent/tls/cert.pem|' \
  -e 's|# Environment=DEPLOY_AGENT_TLS_KEY=.*|Environment=DEPLOY_AGENT_TLS_KEY=/etc/deploy-agent/tls/key.pem|' \
  -e "s|# Environment=DEPLOY_AGENT_BIND_ADDR=.*|Environment=DEPLOY_AGENT_BIND_ADDR=${TS_IP}:8443|" \
  /etc/systemd/system/deploy-agent.service
sudo systemctl daemon-reload
sudo systemctl enable --now deploy-agent
sudo systemctl status deploy-agent --no-pager
EOF
```

The heredoc is quoted (`<<'EOF'`) specifically so `$(tailscale ip -4)`
resolves on the Pi, inside the `ssh` session — not on your local machine,
which would report its own Tailscale IP instead of the Pi's.

### 1.5 — Generator + polkit rule (also one-time, root, reviewed by hand)

```bash
scp deploy-agent/deploy-agent-generator.sh deploy-agent/install-generator.sh pi:/tmp/
ssh pi 'cd /tmp && sudo bash install-generator.sh'
```

Seed the polkit rule from this repo's template (starts with just the
self-unit; every future onboarded project's units get merged in by hand
from `deploy-admin`'s printed snippet — see Part 2):

```bash
scp deploy-agent/polkit/49-deploy-agent.rules pi:/tmp/
ssh pi 'sudo mv /tmp/49-deploy-agent.rules /etc/polkit-1/rules.d/49-deploy-agent.rules'
```

Verify the whole D-Bus/polkit loop per
`docs/04-generator-polkit-verification.md` §4.3–4.4 before moving on —
in particular, confirm there's no separate D-Bus bus-level policy file
also gating `org.freedesktop.systemd1` on your Pi's OS version (open
question #5 in the consolidated design doc is still unresolved; check it
here rather than assuming).

### 1.6 — Onboard the `deploy-agent` self-project

Self is an ordinary project — onboard it the same way as anything else:

```bash
ssh pi 'sudo deploy-admin onboard deploy-agent \
  --units deploy-agent.service \
  --install-dir /opt/deploy-agent'
```

This writes `/etc/deploy-agent/projects/deploy-agent.toml` and generates
`/etc/deploy-agent/secrets/deploy-agent.key`. Read that secret back off
the Pi — you'll need its contents for the `DEPLOY_AGENT_HMAC_KEY` GitHub
secret in Part 3:

```bash
ssh pi 'sudo cat /etc/deploy-agent/secrets/deploy-agent.key'
```

### 1.7 — Tailscale ACLs

In the Tailscale admin console, restrict the CI tag to just this Pi's
deploy port — don't grant `tag:ci` blanket tailnet access:

```json
"acls": [
  {"action": "accept", "src": ["tag:ci"], "dst": ["tag:rpi:8443"]}
]
```

Tag the Pi itself `tag:rpi` (or whatever you used above) when you join it
to the tailnet.

At this point: `deploy-agent` is running, bound to the Pi's Tailscale IP,
self-onboarded, the generator/polkit loop is verified, and you have the
HMAC secret needed for CI. Part 3 covers wiring up `self-deploy.yml` so
`deploy-agent` starts updating itself from here on.

---

## Part 2 — Onboarding a new project

Every project — including future ones, not just the first — goes through
`deploy-admin onboard`, run by hand under `sudo` on the Pi:

```bash
sudo deploy-admin onboard myproj \
  --units myproj-api.service,myproj-worker.service \
  --install-dir /srv/apps/myproj \
  --retain-count 5 \
  --health-check-url http://127.0.0.1:9000/healthz
```

(`--install-dir` defaults to `/srv/apps/<project>` if omitted;
`--health-check-*` are optional and currently parsed but not yet acted on
by the agent — see the consolidated design doc's open questions.)

What this does, and what you do with the output:

1. **Generates an HMAC secret** at `/etc/deploy-agent/secrets/myproj.key`
   (skipped if one already exists — pass `--force` to rotate it). Copy
   its contents into that project's own repo as a GitHub secret (see
   Part 3) — this is the value CI signs bundles with.
2. **Writes `/etc/deploy-agent/projects/myproj.toml`** — always
   regenerated from whatever flags you passed, so re-running `onboard`
   with a different `--units` list is how you add/remove allow-listed
   units later.
3. **Scaffolds `/srv/apps/myproj/{releases,shared}`**, chowned to
   `deploy-agent:deploy-agent` (best-effort — if you didn't run this
   under `sudo`, you'll get a warning instead of a failure, and need to
   `chown` by hand).
4. **Prints the polkit snippet** for `myproj-api.service` and
   `myproj-worker.service`. Paste those lines into the `allowedUnits`
   array in `/etc/polkit-1/rules.d/49-deploy-agent.rules` by hand — this
   file is deliberately never written automatically. No reload needed for
   polkit itself (it watches the rules directory), but the *next* deploy
   that changes those units' content will still need the normal
   generator `daemon-reload`, which `deploy-agent` already triggers on
   its own.

Re-running `onboard` for an existing project is safe: the secret is
preserved, the TOML and directories are just recomputed, and the snippet
reprints — useful when a project adds a new unit later.

**Decommissioning a project is not automated** — remove its
`projects/<name>.toml`, `secrets/<name>.key`, `/etc/deploy-agent/units/<name>/`,
its `allowedUnits` entries, and its `/srv/apps/<name>` tree by hand if you
ever need to.

---

## Part 3 — A downstream repo's own deploy workflow

This is what a *different* repository (not `deploy-agent-workspace`)
needs to deploy through the agent. It builds its own binaries, stages
them, and calls the reusable `deploy.yml` workflow hosted in this repo.

### 3.1 — One-time repo setup (in the downstream repo's GitHub settings)

| Kind | Name | Value |
|---|---|---|
| Secret | `HMAC_KEY` (or any name — matched to what you pass below) | contents of `/etc/deploy-agent/secrets/myproj.key` from Part 2 |
| Secret | `TS_OAUTH_CLIENT_ID` / `TS_OAUTH_CLIENT_SECRET` | a Tailscale OAuth client scoped to `tag:ci` (can reuse the same client across every repo deploying to this Pi, or make one per repo — either works, the ACL scoping is by tag, not by client) |
| Secret | `DEPLOY_CA_CERT` | contents of `ca-cert.pem` from Part 1.3 — same value for every project on this Pi, since it's one TLS identity for the whole agent |
| Variable | `AGENT_HOST` | the Pi's Tailscale MagicDNS name, e.g. `rpi.footnet-lambda.ts.net` |

### 3.2 — The workflow file

`.github/workflows/deploy.yml` in the downstream repo:

```yaml
name: Deploy

on:
  push:
    branches: [main]

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-unknown-linux-gnu

      - name: Install cross-linker
        run: |
          sudo apt-get update
          sudo apt-get install -y gcc-aarch64-linux-gnu

      - uses: Swatinem/rust-cache@v2

      - name: Cross-compile
        run: cargo build --release --target aarch64-unknown-linux-gnu

      - name: Stage bundle contents
        run: |
          mkdir -p staging/bin staging/systemd
          cp target/aarch64-unknown-linux-gnu/release/myproj-api staging/bin/
          cp target/aarch64-unknown-linux-gnu/release/myproj-worker staging/bin/
          cp deploy/myproj-api.service deploy/myproj-worker.service staging/systemd/

      - uses: actions/upload-artifact@v7
        with:
          name: myproj-staging
          path: staging

  deploy:
    needs: build
    uses: <owner>/deploy-agent-workspace/.github/workflows/deploy.yml@v1   # TODO: pin once this repo has a real remote/tag
    with:
      project: myproj
      artifact-name: myproj-staging
      staging-dir: staging
      agent-host: ${{ vars.AGENT_HOST }}
      port: "8443"
    secrets:
      hmac-key: ${{ secrets.HMAC_KEY }}
      tailscale-oauth-client-id: ${{ secrets.TS_OAUTH_CLIENT_ID }}
      tailscale-oauth-client-secret: ${{ secrets.TS_OAUTH_CLIENT_SECRET }}
      deploy-ca-cert: ${{ secrets.DEPLOY_CA_CERT }}
```

Notes on why each piece is there:

- **`build` and `deploy` must stay separate jobs**, connected by
  `actions/upload-artifact` / the reusable workflow's own
  `actions/download-artifact` step (via `artifact-name`) — `needs:` alone
  orders jobs, it doesn't share a filesystem between them.
- `deploy-ci-source` is **not set**, so it defaults to `"download"` —
  correct for any repo other than `deploy-agent-workspace` itself.
  This requires that `release-deploy-ci.yml` has run at least once
  (push a `deploy-ci-v*` tag in `deploy-agent-workspace`) so there's a
  release binary to download; and that `deploy.yml`'s `deploy-ci-repo`
  default (currently a placeholder) has been updated to this repo's real
  `owner/repo` once it has one.
- **Pin the reusable-workflow reference to a real tag** (`@v1`, or an
  exact `@v1.2.0`) once `deploy-agent-workspace` has tagged releases —
  never `@main`, per the design doc's pinning rule: an untested change to
  the shared workflow shouldn't silently break every downstream project's
  next deploy at once.
- `myproj-api.service` / `myproj-worker.service` must already be in
  this project's `allowed_units` from Part 2's `onboard` call, and their
  content must be paste-committed into the polkit rules file — a deploy
  whose manifest lists a unit outside that allow-list is rejected
  outright by `deploy-agent` before anything is touched.
- Health-check polling is not wired into `deploy.yml` yet (see the
  consolidated design doc) — a failed `deploy-ci send` still fails the
  job (non-2xx response), but nothing currently checks the app actually
  came up healthy afterward.
