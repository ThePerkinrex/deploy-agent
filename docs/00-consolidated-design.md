# RPi Deploy Agent — Consolidated Design & Status

This is the current single source of truth for the project: what it's for,
what's been decided, what's actually built, and what's left. It supersedes
`rpi-deploy-agent-handoff.md` and the step guides (`01`–`04`) for "what's
true right now" — those documents remain useful as historical, in-depth
walkthroughs of *why* each piece was built the way it was, and are not
being deleted. Where implementation diverged from what a step doc
describes, this document reflects the code, not the doc.

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
- **`deploy-admin`** — intended to run on the Pi, by hand, under `sudo`,
  for project onboarding (secret generation, project TOML templating,
  `/srv/apps/<project>` scaffolding, printing the polkit snippet for a
  human to paste in). **Currently just the `cargo new` placeholder —
  none of this exists yet.**

## Confirmed design decisions

### Networking
- Tailscale, hosted (free/personal plan) — not Headscale.
- `deploy-agent` binds only to the Tailscale interface, never `0.0.0.0`.
- GitHub Actions uses `tailscale/github-action`, OAuth client scoped to
  `tag:ci`, ephemeral node.
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

**Not yet built**: health-check polling in `deploy-ci`; the reusable
GitHub Actions workflow (`deploy-agent-workspace/.github/workflows/deploy.yml`)
that was meant to wrap Tailscale join + pinned-`deploy-ci`-download +
bundle/send/health-check for downstream repos to call — there is no
`.github/workflows/` directory in this repo at all yet; and the
release/tagging process for distributing a prebuilt `deploy-ci` binary to
downstream repos.

### Project onboarding — `deploy-admin`
Design intent unchanged (manual, interactive, run under `sudo`, never part
of the automated deploy path, prints rather than writes the polkit
snippet). **Not implemented** — the crate is still the `cargo new`
placeholder.

## Current implementation status

| Component | Status | Notes |
|---|---|---|
| `deploy-common::manifest` | Done | Versioned schema, `schema_version` checked explicitly |
| `deploy-common::bundle` | Done | Build/extract/checksum-verify, tar-slip hardening, negative-path tests exist |
| `deploy-common::release` | Done | Atomic swap, prune (never deletes `current`) |
| `deploy-common::hmac` | Done | Signing string, compute/verify, constant-time, unit tests |
| `deploy-common::project` | Done (schema) | `HealthCheckConfig` field parsed but unused downstream |
| `deploy-agent` HTTP + HMAC + unpack/swap | Done | TLS mandatory; body limit still a 200MB placeholder, never hardened |
| `deploy-agent` D-Bus/systemd (`zbus`) | Done | start/stop/restart/reload, `JobRemoved`-aware |
| `deploy-agent` deploy sequence + rollback | Done, evolved | stop-all → reload → start-all, automatic rollback on any failure |
| `deploy-agent` self-update | Done | unit-name match + `exit(0)` + `Restart=always`; startup self-prune |
| Generator script + installer | Done | adds/removes stale symlinks, `set -euo pipefail` |
| Polkit rule | Done, evolved | stop/start/restart/reload-or-restart verbs, unit allow-list |
| `deploy-ci bundle` | Done | |
| `deploy-ci send` | Done | pinned-CA TLS trust |
| `deploy-ci` health-check polling | **Not started** | |
| Local CA / cert tooling (`create_certs.sh`) | Done | not previously documented |
| `deploy-admin` | **Not started** | placeholder crate only |
| Reusable GitHub Actions workflow | **Not started** | no `.github/workflows/` at all |
| `deploy-ci` release/distribution process | **Not started** | |
| `/status` endpoint | **Not started** | |
| Hardening pass (body size limits, etc.) | **Not started** beyond what shipped in Step 1/2 | replay window and tar-slip protection are done; size limit is still the dev placeholder |
| Bootstrap process for first-ever install | **Not started in this repo** | handoff references a prior `send.sh`, not present here |

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
9. **`deploy-ci` release/versioning process — still open.** No git-tag or
   release-workflow automation exists; downstream pinning convention
   (`@v1` vs `@v1.2.0`) undecided.
10. **`deploy-admin` command surface — still open**, and now blocking:
    the crate doesn't exist yet at all, so this is a "start from scratch"
    item, not just a naming decision.

Newly identified gaps (not in the original handoff's list):
- No `.github/workflows/` in this repo — the entire reusable-workflow
  piece of the CI-side design is unbuilt.
- Body size limit hardening was explicitly deferred in step doc 2 and
  never revisited — still a hardcoded 200MB placeholder.
- No `/status` endpoint for retention/release introspection.
- No bootstrap script/runbook for the very first manual install onto a
  fresh Pi exists in this repo (the handoff references reusing an
  existing `send.sh`, which isn't part of this workspace).

## Suggested next steps

Given what's already done (bundle/manifest/release layout, HMAC, D-Bus/
polkit/generator, core deploy+rollback flow, self-update, and basic
`deploy-ci`), the remaining build-order items from the handoff collapse to:

1. **Health checks** — decide the contract (open question #3), implement
   the check in `deploy-agent` (call it after step 7's start-all, before
   pruning) and/or a `deploy-ci health-poll` companion, wiring failure
   into the rollback path that already exists.
2. **`deploy-admin`** — onboarding CLI: secret generation, project TOML
   templating (reusing `deploy-common::project::ProjectConfig`), `/srv/apps/<project>`
   scaffolding, printing the polkit allow-list snippet.
3. **Reusable GitHub Actions workflow** — Tailscale join, pinned
   `deploy-ci` download, `bundle`/`send`/health-poll steps, fail the run
   on non-2xx or failed health check. Wire the first downstream repo to
   call it via a pinned tag.
4. **`deploy-ci` distribution/release process** — tag-triggered build and
   attach of a prebuilt `x86_64-unknown-linux-gnu` binary asset; decide
   the pinning convention for downstream consumers.
5. **Hardening pass** — revisit the 200MB body-size placeholder; confirm
   D-Bus bus-level policy on the real target OS (open question #5); any
   other defense-in-depth gaps found once `deploy-admin` exists and
   real projects are onboarded.
6. **`/status` endpoint** — expose current release, retention state, and
   recent deploy history for observability.

Self-update and automatic rollback, originally later items in the build
order, are already done ahead of schedule (matching step doc 04's own
closing note).
