# RPi Deploy Agent — Consolidated Design & Status

This is the current single source of truth for the project: what it's for,
what's been decided, what's actually built, and what's left. It supersedes
`rpi-deploy-agent-handoff.md` and the step guides (`01`–`04`) for "what's
true right now" — those documents remain useful as historical, in-depth
walkthroughs of *why* each piece was built the way it was, and are not
being deleted. Where implementation diverged from what a step doc
describes, this document reflects the code, not the doc.

For hands-on instructions (bootstrapping the Pi, onboarding a project,
writing a downstream repo's own deploy workflow), see
`docs/05-pi-install-and-project-onboarding-guide.md`. This document stays
the design/status reference; that one is the runbook.

## Goal

A generic, config-driven deployment agent (`deploy-agent`) written in Rust,
running on a Raspberry Pi, that receives pre-compiled release bundles over
HTTP from GitHub Actions and applies them via systemd. Compilation happens
entirely in CI — the Pi never builds anything. The agent is also able to
redeploy *itself* through the same mechanism.

First real project to deploy through it: a Rust workspace with two binary
crates, each with its own systemd unit(s).

## Architecture overview

Four crates in one Cargo workspace (`resolver = "3"`):

- **`deploy-common`** — shared library. Manifest schema, bundle build/
  extract, release-directory layout, HMAC sign/verify byte construction,
  server-side project-policy struct. The only place any of this logic is
  implemented; everything else calls into it.
- **`deploy-agent`** — runs on the Pi. HTTP server (axum + rustls), HMAC
  verification, bundle unpack/stage/swap, D-Bus/systemd integration via
  `zbus`. Ships to the Pi via the deploy pipeline itself, including
  redeploying itself.
- **`deploy-ci`** — runs on the GitHub-hosted runner. Native x86_64 build,
  not cross-compiled. Builds the bundle, signs it, sends it. Currently:
  `bundle` and `send` subcommands. Health-check polling is designed for
  but not yet implemented.
- **`deploy-admin`** — runs on the Pi, by hand, under `sudo`, for project
  onboarding: `deploy-admin onboard <project> --units a.service,b.service`
  generates the HMAC secret (idempotent — preserved across re-runs unless
  `--force`), writes `projects/<name>.toml`, scaffolds
  `<install-dir>/{releases,shared}`, best-effort `chown`s them to the
  agent's system user, and prints (never writes) the polkit allow-list
  snippet for a human to paste in.

## Confirmed design decisions

### Networking
- Tailscale, hosted (free/personal plan) — not Headscale.
- `deploy-agent` binds only to the Tailscale interface, never `0.0.0.0`
  — as of this fix, the bind address is configurable via
  `DEPLOY_AGENT_BIND_ADDR` (`host:port`), defaulting to `127.0.0.1:8443`
  for local dev. In production, set it to the Pi's Tailscale IP (stable
  once assigned — get it once via `tailscale ip -4`) in
  `deploy-agent.service`. Previously this was hardcoded to loopback only
  (`deploy-agent/src/main.rs:59`), which would have silently made the
  agent unreachable over the tailnet; that's what this env var fixes.
- GitHub Actions uses `tailscale/github-action@v4`, OAuth client
  (`oauth-client-id`/`oauth-secret`) scoped to `tag:ci` — nodes created
  this way are automatically ephemeral (Tailscale's own default for
  OAuth-client-authenticated nodes), no extra flag needed.
- Tailscale ACLs restrict `tag:ci` → `tag:rpi:<deploy-port>` only.
- Reached via Tailscale MagicDNS name, not a raw IP.
- TLS is terminated in the agent itself (rustls) even inside the tailnet.
  **As currently implemented, TLS is mandatory** — `deploy-agent/src/main.rs`
  loads cert/key unconditionally at startup and panics if they're missing;
  the plain-HTTP dev fallback described in step doc 2 has been removed.
- `create_certs.sh` (repo root) generates a local CA + server cert with
  proper SANs (MagicDNS name + Tailscale IP) — this tooling isn't
  mentioned in any of the earlier docs but is real and in place.
- `deploy-ci send` trusts a pinned local CA via `--ca-cert` rather than
  disabling verification — a real answer to the "production
  cert-provisioning process" question step doc 2 explicitly deferred.

### Authentication / integrity
- HMAC-SHA256, one secret per project, `/etc/deploy-agent/secrets/<project>.key`.
- Signed string: `timestamp + "\n" + project + "\n" + sha256(body)`, sent as
  `X-Deploy-Project` / `X-Deploy-Timestamp` / `X-Deploy-Signature: sha256=<hex>`.
- 5-minute replay window, constant-time compare (`hmac::Mac::verify_slice`).
- Construction logic lives once in `deploy-common::hmac`, used identically
  by `deploy-ci` (signer) and `deploy-agent` (verifier).

### Server-side policy
`/etc/deploy-agent/projects/<name>.toml` (`ProjectConfig` in
`deploy-common::project`): install dir, allow-listed unit names,
retention count, optional health-check config (parsed, **not yet
consumed**), path to the HMAC secret.

### Bundle format & release layout
Unchanged from the original design: `tar.zst` with `manifest.toml`, `bin/`,
`systemd/`, `config/`; Capistrano-style `releases/<ts>_<sha>/` +
`current` symlink + `shared/`. Manifest is versioned (`schema_version`),
every binary/unit carries its own sha256, checked after extraction and
before the symlink swap. Archive extraction sanitizes paths and rejects
all symlink/hardlink entries (three-layer defense: component inspection,
link-type rejection, post-join canonicalization check).

### Deploy sequence — updated from the original per-unit-restart design
The original design (and step docs 3–4) restarted each unit individually.
**As implemented, the sequence is stricter and different:**

1. Extract to staging, verify checksums, cross-check manifest project
   against the signed header.
2. **Reject the entire deploy upfront if any manifest unit isn't in that
   project's `allowed_units`** — not a per-unit skip-with-a-warning as the
   step docs described, a hard failure before anything is touched.
3. Move the verified release into `releases/`, sync unit files into
   `/etc/deploy-agent/units/<project>/`.
4. Atomically swap `current`.
5. **Stop every managed unit and wait for `JobRemoved`** — done first, for
   all units, so two versions of the same program are never running
   concurrently.
6. If unit file content changed, `daemon-reload` (waits for completion).
7. **Start every managed unit and wait for `JobRemoved`.**
8. Prune old releases (skipped for self-updates).

**Automatic rollback** is wired in at every failure point in steps 5–7:
swap `current` back to the previous release and restart the previous
units. This resolves both open design questions step doc 3 posed
("blocking vs async completion signal" → blocking on `JobRemoved`;
"automatic rollback vs external health check" → automatic rollback is
implemented, in addition to — not instead of — future health checks).

### Self-update
Confirms handoff open question #4: self *is* an ordinary project config,
not a special-cased code path — gated by comparing each manifest unit's
name against `DEPLOY_AGENT_UNIT_NAME` (env var, default
`deploy-agent.service`). A matching unit's restart is deferred: instead of
going through D-Bus, the handler finishes writing the HTTP response, then
calls `std::process::exit(0)` (after a short delay), relying on the unit's
`Restart=always` to bring the new binary up (`ExecStart` follows the
`current` symlink). The agent also self-prunes old `deploy-agent` releases
on startup.

### Privilege model — D-Bus + polkit, no root, no sudo for the agent
Unchanged in shape from the handoff: `deploy-agent` runs as a dedicated
non-root user, talks to `org.freedesktop.systemd1` over D-Bus via `zbus`,
subscribes to `JobRemoved` for completion status. Implemented calls:
`StartUnit`, `StopUnit`, `RestartUnit`, `Reload` (all used — `RestartUnit`
is available but the deploy flow itself uses stop-then-start rather than
restart, per above).

**Polkit rule** (`deploy-agent/polkit/49-deploy-agent.rules`) has already
been updated beyond what step doc 04 shows: `reload-daemon` is granted
unconditionally; `manage-units` requires both the unit to be in a hardcoded
allow-list *and* the verb to be one of `stop`, `start`, `restart`,
`reload-or-restart` (doc 04 only allowed `restart`/`reload-or-restart` —
extended to `stop`/`start` to match the actual stop-then-start deploy
flow). This is still a hand-edited file today; `deploy-admin` is meant to
print (not write) the per-project snippet for a human to paste in, but
that tool doesn't exist yet.

### Unit file provisioning — systemd generator
Unchanged from the confirmed design: `deploy-agent` writes unit content to
`/etc/deploy-agent/units/<project>/<unit>.service` (no elevated
permissions needed); a small root-owned generator
(`deploy-agent/deploy-agent-generator.sh`, installed via
`install-generator.sh`) mirrors/symlinks these into
`/run/systemd/generator/` on every reload, **and removes stale symlinks**
for units whose source file is gone (a gap identified and closed in step
doc 04, present in the current script). `set -euo pipefail` so a broken
`UNITS_DIR` state fails loudly.

### CI-side tooling and distribution
`deploy-ci` exists as a separate native x86_64 CLI crate (not a
subcommand of `deploy-agent`), using `deploy-common` directly — no second
implementation of manifest-building or HMAC signing. Currently implements:
- `deploy-ci bundle` — hashes `bin/` and `systemd/`, writes `manifest.toml`,
  builds the `.tar.zst`.
- `deploy-ci send` — HMAC-signs and POSTs a bundle, with pinned-CA TLS
  trust (`--ca-cert`) as an explicit opt-in over the system root store.

**Target correction**: the handoff originally suggested
`aarch64-unknown-linux-musl` for cross-compiled binaries (to sidestep
glibc version drift). The confirmed real target environment is
**`aarch64-unknown-linux-gnu`** — all cross-compilation (`cross build
--target aarch64-unknown-linux-gnu`) uses this triple instead.

**Now implemented**: three workflows under `.github/workflows/`:
- `deploy.yml` — the reusable `workflow_call` workflow. Connects to the
  tailnet, downloads the caller's staged build via
  `actions/download-artifact` (the caller's build job must upload it
  first — `needs:` alone does not share files between jobs, a gap in the
  handoff's original downstream-repo sketch that this closes), obtains
  `deploy-ci` (see below), runs `bundle` then `send` with `--ca-cert`
  pinning. Inputs renamed from the handoff's ambiguous `tailnet` to
  `agent-host` (the full MagicDNS hostname) since the original implied
  concatenating a hardcoded `"rpi."` prefix.
- `release-deploy-ci.yml` — on `deploy-ci-v*` tag push, builds native
  x86_64 `deploy-ci` and attaches it to a GitHub Release. This is the
  minimum slice of open question #9 needed to make the workflow usable —
  not the full versioning-policy decision.
- `self-deploy.yml` — this repo's own workflow: cross-compiles
  `deploy-agent` for `aarch64-unknown-linux-gnu`, stages
  `deploy-agent.service` alongside the binary (needed on every self-deploy
  even when the unit's content hasn't changed — self-update detection
  keys off the manifest listing a unit named `deploy-agent.service`), and
  calls `deploy.yml` to actually deploy itself. Exercises the self-update
  path via CI, not just local `curl`.

`deploy-ci`'s distribution split, per an explicit decision this round:
**build from source** when `deploy.yml` is invoked locally from within
this repo (`self-deploy.yml`'s `deploy-ci-source: build`, plain
`actions/checkout@v4` with no repo/ref override since it's already the
right commit); **download** a pinned-or-`latest` release binary
(`deploy-ci-source: download`, the default) for every other, downstream
repo. `deploy-ci-repo` defaults to a placeholder org/repo string — this
repo has no git remote configured yet, so downstream examples need that
value filled in once one exists.

**Still not built**: health-check polling in `deploy-ci`/`deploy.yml` (no
step calls it, per the deferral); the actual repo-level secrets/variables
(`DEPLOY_AGENT_HOST`, `DEPLOY_AGENT_HMAC_KEY`, `TS_OAUTH_ID`,
`TS_OAUTH_SECRET`, `DEPLOY_CA_CERT`) `self-deploy.yml` depends on — those
are one-time GitHub repo configuration, not something a code change can
create.

### Project onboarding — `deploy-admin`
**Implemented**: `deploy-admin/src/main.rs`, a single `onboard`
subcommand exactly matching the design intent (manual, interactive, run
under `sudo`, never part of the automated deploy path). Verified locally
against a scratch config root: generates a 32-byte HMAC secret at mode
`0600` unless one already exists (idempotent; `--force` regenerates),
(re)writes `projects/<name>.toml` from `deploy_common::project::ProjectConfig`
every run, scaffolds `<install-dir>/{releases,shared}`, best-effort
`chown -R`s the install dir and project files to the configured
agent user/group (warns rather than aborting if not running as root —
lets it be exercised without `sudo` during development), and prints the
polkit allow-list snippet for just that project's units. No
special-casing for the `deploy-agent` self-project — it's onboarded the
same way (`deploy-admin onboard deploy-agent --units
deploy-agent.service --install-dir /opt/deploy-agent`), confirming it
really is "just another project" end to end, including in the tooling.

## Current implementation status

| Component | Status | Notes |
|---|---|---|
| `deploy-common::manifest` | Done | Versioned schema, `schema_version` checked explicitly |
| `deploy-common::bundle` | Done | Build/extract/checksum-verify, tar-slip hardening, negative-path tests exist |
| `deploy-common::release` | Done | Atomic swap, prune (never deletes `current`) |
| `deploy-common::hmac` | Done | Signing string, compute/verify, constant-time, unit tests |
| `deploy-common::project` | Done (schema) | `HealthCheckConfig` field parsed but unused downstream |
| `deploy-agent` HTTP + HMAC + unpack/swap | Done | TLS mandatory; bind address configurable via `DEPLOY_AGENT_BIND_ADDR` (defaults `127.0.0.1:8443`, set to the Pi's Tailscale IP in production); body limit still a 200MB placeholder, never hardened |
| `deploy-agent` D-Bus/systemd (`zbus`) | Done | start/stop/restart/reload, `JobRemoved`-aware |
| `deploy-agent` deploy sequence + rollback | Done, evolved | stop-all → reload → start-all, automatic rollback on any failure |
| `deploy-agent` self-update | Done | unit-name match + `exit(0)` + `Restart=always`; startup self-prune |
| Generator script + installer | Done | adds/removes stale symlinks, `set -euo pipefail` |
| Polkit rule | Done, evolved | stop/start/restart/reload-or-restart verbs, unit allow-list |
| `deploy-ci bundle` | Done | |
| `deploy-ci send` | Done | pinned-CA TLS trust |
| `deploy-ci` health-check polling | **Not started** | deferred |
| Local CA / cert tooling (`create_certs.sh`) | Done | not previously documented |
| `deploy-admin onboard` | Done | secret gen (idempotent), project TOML, dir scaffolding, best-effort chown, polkit snippet print |
| Reusable GitHub Actions workflow (`deploy.yml`) | Done | fixes the missing artifact hand-off in the handoff's original sketch |
| `deploy-ci` release workflow (`release-deploy-ci.yml`) | Done | tag-triggered, native x86_64 build, attached to a GitHub Release |
| Self-deploy workflow (`self-deploy.yml`) | Done | cross-compiles for `aarch64-unknown-linux-gnu`, deploys via `deploy.yml` |
| `deploy-ci` release/distribution *policy* (open q. #9) | Partial | mechanism exists; pinning convention / semver discipline still undecided |
| `/status` endpoint | **Not started** | deferred |
| Hardening pass (body size limits, etc.) | **Not started** beyond what shipped in Step 1/2 | deferred; replay window and tar-slip protection are done, size limit is still the dev placeholder |
| Bootstrap process for first-ever install | **Not started in this repo** | deferred; handoff references a prior `send.sh`, not present here |

## What's missing / open questions

Carried over from the handoff's original list, updated with current status:

1. ~~Exact unit directory path~~ — **Resolved**: `/etc/deploy-agent/units/<project>/<unit>.service`.
2. ~~Manifest schema~~ — **Resolved and implemented**, versioned.
3. **Health-check contract — still open.** No code consumes
   `HealthCheckConfig` yet; `deploy-ci` has no poll-health step. Needs a
   decision (plain port poll vs. required `/healthz` shape) and
   implementation on both sides.
4. ~~Self as ordinary project config~~ — **Resolved**: yes, ordinary
   config + unit-name special case in the runtime code.
5. **Raspberry Pi OS version/arch, D-Bus system-bus defaults — still
   open.** No evidence in the repo that this has been verified against a
   real device. Step doc 04's §4.3 verification procedure (checking for a
   `<policy user="deploy-agent">` stanza needed in
   `org.freedesktop.systemd1.conf`) should be run once there's real
   hardware to test against, if not already done outside version control.
6. ~~Polkit action IDs~~ — **Resolved**, and the rule already covers more
   verbs than doc 04 specified (stop/start added).
7. ~~Generator script scope~~ — **Resolved**: symlink-only, no templating,
   self-cleaning of stale links.
8. **Tailscale free-tier limits — still open**, no code artifact either way.
9. **`deploy-ci` release/versioning process — partially resolved.**
   `release-deploy-ci.yml` now builds and publishes a `deploy-ci` binary
   on any `deploy-ci-v*` tag push, and `deploy.yml` can download it
   pinned-or-`latest`. What's still undecided: the actual pinning
   convention downstream repos should standardize on (`@v1` floating-major
   vs. exact `@v1.2.0`), and whether semver discipline is maintained on
   the reusable workflow's `with:`/`secrets:` inputs.
10. ~~`deploy-admin` command surface~~ — **Resolved and implemented**: a
    single idempotent `onboard` subcommand (see above).

Newly identified gaps (not in the original handoff's list):
- Body size limit hardening was explicitly deferred in step doc 2 and
  never revisited — still a hardcoded 200MB placeholder.
- No `/status` endpoint for retention/release introspection.
- No bootstrap script/runbook for the very first manual install onto a
  fresh Pi exists in this repo (the handoff references reusing an
  existing `send.sh`, which isn't part of this workspace).
- `deploy.yml`'s `deploy-ci-repo` default is a placeholder org/repo string
  — this repo has no git remote configured, so it needs a real value once
  one exists, before any downstream repo can actually use `download` mode.
- `self-deploy.yml` depends on repo-level secrets/variables
  (`DEPLOY_AGENT_HOST`, `DEPLOY_AGENT_HMAC_KEY`, `TS_OAUTH_ID`,
  `TS_OAUTH_SECRET`, `DEPLOY_CA_CERT`) that must be configured by hand in
  GitHub's repo settings — no automation creates these.
- None of the new workflow YAML has been run or linted (no `actionlint`/
  `gh`/`cross` available in this environment) — reviewed by hand only.

## Suggested next steps

With `deploy-admin` and the CI workflows now in place (this pass), an MVP
end-to-end path — onboard a project by hand, push to CI, land on the Pi —
exists in code. What's left, in rough priority order:

1. **One-time setup to actually exercise it**: a real git remote/org for
   this repo (needed for `deploy.yml`'s `deploy-ci-repo` default and any
   downstream `uses:` reference), the `self-deploy.yml` secrets/variables,
   an initial manual bootstrap of `deploy-agent` on the Pi (still
   out-of-band by design), and running `deploy-admin onboard deploy-agent`
   for the self-project once that bootstrap exists.
2. **Health checks** — decide the contract (open question #3), implement
   the check in `deploy-agent` (call it after the deploy sequence's
   start-all step, before pruning) and/or a `deploy-ci health-poll`
   companion, wiring failure into the rollback path that already exists.
3. **Hardening pass** — revisit the 200MB body-size placeholder; confirm
   D-Bus bus-level policy on the real target OS (open question #5); any
   other defense-in-depth gaps found once real projects are onboarded.
4. **`/status` endpoint** — expose current release, retention state, and
   recent deploy history for observability.
5. **`deploy-ci` pinning convention** — decide `@v1` vs exact-tag pinning
   for downstream repos, and whether the reusable workflow's inputs/
   secrets get semver discipline.

Self-update, automatic rollback, `deploy-admin`, and the CI workflows —
all originally later/undone items in the build order — are now done
ahead of the handoff's original sequencing.
