# RPi Deploy Agent — Project Handoff

## Goal

A generic, config-driven deployment agent (`deploy-agent`) written in Rust, running on a Raspberry Pi, that receives pre-compiled release bundles over HTTP from GitHub Actions and applies them via systemd. Compilation happens entirely in CI — the Pi never builds anything. The agent must also be able to redeploy *itself* through the same mechanism.

First real project to deploy through it: a Rust workspace with two binary crates, each with its own systemd unit(s).

## Confirmed decisions (do not re-litigate unless something concrete changes)

### Networking
- **Tailscale**, hosted (free/personal plan) — not Headscale. Revisit self-hosting only if device limits or philosophical objections become a real issue.
- The `deploy-agent` binds only to the Tailscale interface (its `100.x.x.x` IP or `tailscale0`), never `0.0.0.0`. Not reachable from the public internet under any circumstance.
- GitHub Actions workflow uses the official `tailscale/github-action`, authenticating via an **OAuth client** scoped to `tag:ci`, requesting an **ephemeral** node (auto-removed from the tailnet after the job ends).
- Tailscale ACLs restrict `tag:ci` → `tag:rpi:<deploy-port>` only. No blanket tailnet access for CI runners.
- The Pi is reached via its Tailscale **MagicDNS** name (e.g. `rpi.<tailnet>.ts.net`), not a raw IP.
- TLS is still terminated in the agent itself (rustls, self-signed cert from a local CA) even though traffic is already inside the tailnet — belt and suspenders.

### Authentication / integrity
- **HMAC-SHA256**, one secret per project, stored at `/etc/deploy-agent/secrets/<project>.key` (mode 600).
- Signed content is NOT just the raw body — it's `timestamp + "\n" + project + "\n" + sha256(body)`, sent as headers:
  - `X-Deploy-Project`
  - `X-Deploy-Timestamp` (unix seconds)
  - `X-Deploy-Signature: sha256=<hex>`
- Reject if `|now - timestamp| > 5 minutes` (replay window). Constant-time compare (`hmac::Mac::verify_slice`).
- Runtime secrets (DB passwords, API keys for the *deployed apps*) never travel through the deploy pipeline. They live once on the Pi at `/srv/apps/<project>/shared/.env`, referenced by the unit's `EnvironmentFile=`, and persist across releases.
- The signing/construction logic (exact byte sequence signed, header names, hex casing) lives in `deploy-common` and is used identically by `deploy-ci` (signer) and `deploy-agent` (verifier) — never reimplemented in shell, to avoid the two sides silently drifting out of byte-for-byte agreement.

### Server-side policy (per project, source of truth — not trusted from the bundle)
`/etc/deploy-agent/projects/<name>.toml` defines:
- Install directory
- **Allow-listed systemd unit names** the agent is permitted to touch for that project (manifest can't restart arbitrary units)
- Release retention count
- Optional health-check URL/port + timeout
- Path to that project's HMAC secret

### Bundle format (CI → Pi)
`tar.zst` containing:
```
manifest.toml       # git sha, built_at, binaries[], units[]
bin/
  <binary files>
systemd/
  <unit templates>
config/              # non-secret config only
```
- Archive extraction must sanitize paths (reject `..`, absolute paths, symlinks escaping the target dir) — treat as untrusted input despite HMAC verification.
- Manifest schema is explicitly versioned (`schema_version` field), and both the bundle *builder* (`deploy-ci`, CI-side) and the *reader* (`deploy-agent`, Pi-side) share the exact same `Manifest` struct via the `deploy-common` crate — this is why the manifest is built by a small Rust CLI rather than hand-written in bash (see "CI-side tooling" below).

### Release layout on disk (Capistrano-style atomic swap)
```
/srv/apps/<project>/
  releases/
    2026-09-09T14-03-00Z_<sha>/
      bin/
      config/
    2026-09-08T09-11-02Z_<sha>/   (kept per retention count)
  current -> releases/<latest>/
  shared/
    .env
```
- systemd `ExecStart=` points at `.../current/bin/<name>` — the stable symlink, never a concrete release path.
- Deploy sequence: unpack to new `releases/<new>/` → verify checksums from manifest → atomic symlink swap of `current` → restart only allow-listed units → health check → rollback (symlink back + restart) on failure → prune old releases beyond retention.

### Self-update (the agent redeploying itself)
- `deploy-agent` runs under its own systemd unit: `ExecStart=/opt/deploy-agent/current/bin/deploy-agent`, `Restart=always`.
- Same `current`-symlink release mechanism as any other project.
- On a self-targeting deploy: unpack, verify, swap `current` — then **finish writing the HTTP response, then cleanly exit** (`std::process::exit(0)`) rather than calling `systemctl restart` on itself. systemd's `Restart=always` brings it back up as the new binary (since `ExecStart` follows the symlink).
- Bootstrapping (very first install) is out of band — reuse the existing `send.sh`/scp approach once, manually, to get the initial binary + unit in place. Not a solved problem within the agent itself.

### Privilege model — CONFIRMED: D-Bus + polkit, no root, no sudo for the agent process

`deploy-agent` runs as a **dedicated non-root system user** and talks to systemd directly over **D-Bus** instead of shelling out to `systemctl`. This replaces both previously-considered options (root+sandboxing, sudoers+wrapper) with something narrower.

**D-Bus interface**: system bus, service `org.freedesktop.systemd1`, object path `/org/freedesktop/systemd1`, interface `org.freedesktop.systemd1.Manager`. Methods used: `StartUnit(name, mode)`, `StopUnit(name, mode)`, `RestartUnit(name, mode)`/`ReloadOrRestartUnit(name, mode)`, `Reload()` (daemon-reload equivalent). Subscribe to the `JobRemoved` signal to know definitively when a job completed and whether it succeeded, rather than polling `systemctl status` output.

**Rust crate**: `zbus` (pure Rust, async, no libdbus dependency to manage on the Pi) — generate/hand-write a proxy against the systemd1 Manager interface.

**Authorization — polkit, per-unit granularity**: a polkit JS rule in `/etc/polkit-1/rules.d/` inspects the `deploy-agent` user and the specific unit name being acted on, and only allows `StartUnit`/`RestartUnit`/`StopUnit` for units on that project's allow-list:

```js
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        subject.user == "deploy-agent") {
        var allowed = ["myproj-worker.service", "myproj-api.service", "deploy-agent.service"];
        if (allowed.indexOf(action.lookup("unit")) != -1) {
            return polkit.Result.YES;
        }
        return polkit.Result.NO;
    }
});
```

This makes the allow-list a real OS-enforced gate (systemd/polkit refuses the D-Bus call outright for any unit not listed) rather than something only the agent's own code checks before acting.

`Reload()` (daemon-reload) is a separate, coarser polkit action (not per-unit) — granted once to the `deploy-agent` user. Needed whenever unit *content* changes, not just binaries.

**What D-Bus does NOT solve on its own**: writing unit *files* into a systemd search directory (e.g. `/etc/systemd/system/`) is a filesystem operation, orthogonal to D-Bus, and still requires some root-owned step. See "Unit file provisioning" below — this is where the generator design comes in.

**Still open**: confirm D-Bus system-bus access defaults permit the `deploy-agent` user to talk to `org.freedesktop.systemd1` at all on the target Raspberry Pi OS version (usually fine by default, but verify rather than assume).

### Unit file provisioning — CONFIRMED: systemd generator (Option B)

The *system* unit search path is a fixed, compiled-in list of directories (`/etc/systemd/system`, `/run/systemd/system`, `/usr/lib/systemd/system`, etc.) — unlike a *user* manager, there's no config knob to add an arbitrary extra search directory for the system manager. Two systemd-native ways around this were considered; **the generator approach (Option B) was chosen** over provisioning symlinks manually at project onboarding (Option A), trading a small extra root-owned component for fully dynamic, no-manual-step unit provisioning.

**How it works**: a **generator** is a small executable placed in `/etc/systemd/system-generators/`, which systemd runs automatically **as root**, on every `daemon-reload` and at boot. Generators can write unit files/symlinks into `/run/systemd/generator/` — a directory that *is* in the system search path.

**Design**:
- `deploy-agent` writes/owns real unit file content in its own directory, e.g. `/etc/deploy-agent/units/<project>/<unit>.service` (or similar — finalize exact path during implementation). No elevated permission needed for this write.
- A small, fixed, auditable generator script (installed once, root-owned, effectively static — not touched by routine deploys) scans that agent-owned directory and mirrors/symlinks its contents into `/run/systemd/generator/` on every reload.
- After a deploy that adds/changes unit content, `deploy-agent` calls `Reload()` over D-Bus (see above) to trigger systemd to re-run generators and pick up the change — no manual symlinking step required, even for brand-new unit names.
- The generator script itself is the one piece of root-owned code in this path — keep it minimal and treat it as a reviewed, rarely-changed artifact (it is NOT part of the routine deploy hot path and is not itself redeployed through the same pipeline as application code, to avoid a circular trust problem).

**Net effect**: `deploy-agent` the process never needs root or sudo for any part of the systemd-interaction surface. Root-owned code is reduced to: (a) the generator script, fixed and reviewed separately, and (b) whatever one-time OS-level setup grants the `deploy-agent` user its D-Bus/polkit permissions in the first place, and (c) `deploy-admin`, the manual onboarding tool (see below), which runs interactively under `sudo` by a human, not automatically.

### Workspace layout — CONFIRMED: four crates, shared core

```
deploy-agent-workspace/
  deploy-common/   # Manifest schema, bundle build/extract, release layout,
                    # HMAC sign/verify byte-construction. Shared by all three
                    # binaries below — this is the single source of truth
                    # for anything that must match byte-for-byte across
                    # machines (CI runner, Pi).
  deploy-agent/    # Runs ON THE PI. HTTP server, D-Bus/systemd integration.
                    # Ships to end-user Pi via the deploy pipeline itself.
  deploy-ci/       # Runs ON THE GITHUB RUNNER. Builds the bundle, signs it,
                    # sends it, polls health check. Native x86_64 build, no
                    # cross-compilation needed (unlike the app binaries it bundles).
  deploy-admin/    # Runs ON THE PI, BY HAND, UNDER SUDO. Project onboarding —
                    # see "Project onboarding" below. Never runs unattended,
                    # never invoked by the deploy pipeline itself.
```

`deploy-common` is deliberately the only place manifest shape, checksum format, and HMAC signing-string construction are implemented — everything else calls into it rather than reimplementing any of that logic locally (including in shell).

### CI-side tooling and distribution — CONFIRMED: Option C, reusable workflow + prebuilt `deploy-ci` binary

**Why not plain bash for bundle-building**: the bundle-builder has to emit a `manifest.toml` that `deploy-agent` parses with `serde` against an exact `Manifest` struct (field names, RFC 3339 timestamp format, hex-case sha256, `schema_version` semantics). Hand-writing that in bash means a second, untyped, uncompiled reimplementation of the struct that has to be kept in lockstep by hand every time `Manifest` changes in Rust — nothing catches drift until a deploy fails with a serde parse error. Same reasoning applies to the HMAC signing-string construction (`timestamp + "\n" + project + "\n" + sha256(body)`) — easy to get subtly wrong via `printf`/`sha256sum`/`openssl dgst`, worth writing once and testing.

**Decision**: bundle-building, signing, sending, and health-check polling live in `deploy-ci`, a small Rust CLI that depends on `deploy-common` directly (`manifest::Manifest`, `bundle::build_bundle`, the shared HMAC helper) — no second implementation, compiler-checked against the same types the agent uses.

`deploy-ci` runs on the GitHub-hosted runner (x86_64 Linux), **not** on the Pi and **not** as part of what gets cross-compiled — it's a plain native `cargo build --release -p deploy-ci`, no `cross`, no musl toolchain, cacheable like any other native build (e.g. via `Swatinem/rust-cache`). It is kept as a separate crate from `deploy-agent` (not a subcommand of the on-Pi binary) so the binary that actually ships to and runs on the Pi doesn't carry an HTTP client, CLI-parsing, or bundle-building code it never needs at runtime — keeps the on-Pi binary smaller and its surface tighter.

**Distribution to arbitrary downstream repos — CONFIRMED: Option C (prebuilt binary + reusable workflow), not source vendoring**

Three options were considered for how a *new, unrelated* project's repo gets access to `deploy-ci` and the deploy steps:
- Option A (rejected): downstream repo checks out `deploy-agent-workspace` as a second checkout and builds `deploy-ci` from source on every deploy. Rejected — pays a native compile per deploy per downstream repo for a CLI that mostly hasn't changed, and couples every downstream workflow to the exact source layout of the workspace repo.
- Option B (subsumed into C): tag releases of `deploy-agent-workspace` (e.g. `deploy-ci-v1.2.0`), attach a prebuilt `x86_64-unknown-linux-gnu` binary as a release asset, downstream workflows `curl` it down by pinned version. No compile step downstream. This is the right transport mechanism but was upgraded to C for the workflow-packaging reason below.
- **Option C (CHOSEN)**: everything in B, plus the *workflow steps themselves* (Tailscale join, downloading the pinned `deploy-ci` binary, running `bundle`/`send`, failing the run on non-2xx or failed health check) are centralized in a **reusable GitHub Actions workflow** (`workflow_call`) hosted in `deploy-agent-workspace/.github/workflows/deploy.yml`. Downstream repos call it rather than copy-pasting the Tailscale/HMAC dance into every project's own workflow file.

Downstream project's `.github/workflows/deploy.yml` becomes short — it only describes *that project's own build*, plus a call into the shared workflow:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Cross-compile
        run: cross build --release --target aarch64-unknown-linux-musl
      - name: Stage bundle contents
        run: |
          mkdir -p staging/bin staging/systemd staging/config
          cp target/aarch64-unknown-linux-musl/release/myapp staging/bin/
          cp deploy/myapp.service staging/systemd/

  deploy:
    needs: build
    uses: yourorg/deploy-agent-workspace/.github/workflows/deploy.yml@v1
    with:
      project: myapp
      staging-dir: staging
      tailnet: yourtailnet.ts.net
      port: "8443"
    secrets:
      hmac-key: ${{ secrets.DEPLOY_HMAC_KEY }}
      tailscale-oauth-client-id: ${{ secrets.TS_OAUTH_ID }}
      tailscale-oauth-client-secret: ${{ secrets.TS_OAUTH_SECRET }}
```

**Pinning rule**: downstream workflows pin an explicit tag (`@v1.2.0`, or at loosest a major-version tag `@v1` if a semver-discipline release process is maintained) — never `@main`. A reusable workflow that floats to whatever's on `main` in `deploy-agent-workspace` means an untested change to the shared deploy tooling can silently break every downstream project's next deploy simultaneously. Improving the deploy flow later (e.g. retry-with-backoff on health check) is picked up by downstream repos deliberately bumping the pinned tag, not automatically.

### Project onboarding — CONFIRMED: `deploy-admin`, manual/interactive, run under `sudo` on the Pi

Onboarding a new project currently requires several manual, error-prone-by-hand, root-touching steps that are **not** automated by the deploy pipeline itself (deliberately — this mirrors the existing "agent bootstrapping is out of band" decision, and for the same circular-trust reasons the generator script is kept out of the routine deploy hot path):

1. Generate a new HMAC secret and write it to `/etc/deploy-agent/secrets/<project>.key` (mode 600).
2. Template and write `/etc/deploy-agent/projects/<project>.toml` (install dir, allow-listed units, retention count, health-check URL).
3. Create `/srv/apps/<project>/{releases,shared}` with correct ownership.
4. Print the polkit allow-list snippet for that project's units, for a human to paste into `/etc/polkit-1/rules.d/` and review — **not written automatically**, consistent with treating the polkit rule as reviewed, rarely-changed, root-owned config rather than something routine tooling edits unattended.
5. (Not the agent's job, called out separately below) confirm/update the Tailscale ACL if the new project needs anything beyond the existing `tag:ci` → `tag:rpi:<port>` rule — normally a no-op since the ACL is already scoped generically by port, not per-project.

**Decision**: this becomes `deploy-admin`, a fourth workspace crate, depending on `deploy-common` for anything shape-related (e.g. writing a project TOML in the exact format `deploy-agent` expects to parse). It:
- Runs **on the Pi**, **by hand**, **under `sudo`** — an interactive/semi-interactive CLI, not a daemon, not network-facing, not invoked by `deploy-agent` or by any CI workflow.
- Is explicitly **not** part of the automated deploy hot path — same trust-boundary reasoning as the generator script: a tool that can write HMAC secrets, project policy, and print polkit config is exactly the kind of thing that should require a human present, not be reachable via the same pipeline that ships arbitrary project binaries.
- Prints rather than auto-applies the polkit snippet, so a human reviews the diff before it takes effect (`daemon-reload` still required afterward for polkit to pick it up, and for the systemd generator to re-run if any unit content changed).

### Suggested crates
- `axum` + `tokio` — HTTP server (`deploy-agent`)
- `hmac` + `sha2` — signature construction/verification (`deploy-common`, used by both `deploy-agent` and `deploy-ci`)
- `tar` + `zstd` / `async-compression` — bundle build/unpacking (`deploy-common`)
- `serde` + `toml` — config/manifest parsing (`deploy-common`)
- `tracing` + `tracing-subscriber` — journald-friendly logging (`deploy-agent`)
- `anyhow` / `thiserror` — errors
- `rustls` via `axum-server` — in-process TLS, no nginx (`deploy-agent`)
- `zbus` — D-Bus client for talking to `org.freedesktop.systemd1` directly (start/stop/restart/reload units, subscribe to `JobRemoved` for completion/success signals) instead of shelling out to `systemctl` (`deploy-agent`)
- `clap` — CLI parsing (`deploy-ci`, `deploy-admin`)
- `ureq` — blocking HTTP client for the one-shot CI-side send/health-check calls (`deploy-ci`); no need to pull in `tokio` for a CLI that makes a handful of sequential requests and exits

### CI side (GitHub Actions)
- Cross-compile via `cross`, targeting **`aarch64-unknown-linux-musl`** (static linking avoids glibc version drift between CI and whatever Raspberry Pi OS/Debian is on the Pi). Confirm actual Pi architecture/OS before locking this in. This applies only to the *application binaries being deployed* (and eventually `deploy-agent` itself) — `deploy-ci` runs on the runner and is a native x86_64 build, not cross-compiled.
- Workflow: `tailscale/github-action` (OAuth client secrets, `tag:ci`) → build (downstream-repo-specific) → hand off to the shared reusable workflow (`deploy-agent-workspace/.github/workflows/deploy.yml@<pinned-version>`), which downloads the pinned `deploy-ci` binary → bundles → HMAC-signs → sends to `https://rpi.<tailnet>.ts.net:<port>/deploy` → polls health check.
- Non-2xx or failed health check should fail the workflow run.

## Open questions to resolve during implementation

1. Exact path/naming for the agent-owned unit directory the generator reads from (e.g. `/etc/deploy-agent/units/<project>/<unit>.service`) — finalize during implementation.
2. Exact `manifest.toml` schema (field names, versioning so future manifest changes don't break older agent versions).
3. Health-check contract: does the agent just poll a port, or does the deployed app need to expose a specific `/healthz` shape?
4. What "self" looks like as a project config — does `deploy-agent` have its own `projects/deploy-agent.toml`, or is self-update a special-cased code path? (Leaning toward: it should be an ordinary project entry, to keep the mechanism uniform — confirm this holds up once real config schema is written.) If it's ordinary, does `deploy-admin` onboard `deploy-agent` itself the same way it onboards any other project, or is that still part of the separate manual bootstrap step?
5. Raspberry Pi OS version/architecture — confirms the musl target and systemd version assumptions, and needs verifying for D-Bus system-bus defaults (does the `deploy-agent` user have system-bus access out of the box, or does that need explicit `dbus` policy config too, alongside polkit?).
6. Exact polkit action ID(s) for `Reload()` vs `StartUnit`/`RestartUnit`/`StopUnit` — confirm against the systemd version actually shipped on the target OS (action IDs/granularity have evolved across systemd releases).
7. Generator script scope/format — decide whether it symlinks unit files verbatim into `/run/systemd/generator/` or does any transformation (e.g. template substitution); keep it as minimal as possible since it's the one root-owned, non-redeployed component in the system.
8. Actual Tailscale free-tier limits at time of implementation (worth a quick check — pricing/limits have shifted before).
9. Release/versioning process for `deploy-ci` itself — plain git tags with manually-attached binary assets, or a small release workflow in `deploy-agent-workspace` that builds and attaches the asset on tag push? Also decide the pinning convention downstream repos should use (`@v1` floating-major vs. exact `@v1.2.0`) and whether that requires maintaining semver discipline on the reusable workflow's `with:`/`secrets:` inputs.
10. `deploy-admin`'s exact command surface (e.g. `deploy-admin onboard <project> --units a.service,b.service --health-check-url ...`) and output format for the polkit snippet (raw JS fragment to paste vs. a full replacement file with a diff) — finalize during implementation.

## Suggested build order

1. Bundle format + manifest schema + release/symlink directory convention
2. Minimal agent: single endpoint, HMAC verify, unpack → stage → symlink swap (no systemd yet — verify by hand)
3. D-Bus integration via `zbus`: connect to `org.freedesktop.systemd1`, implement start/stop/restart/reload calls, subscribe to `JobRemoved` for completion status
4. Write the generator script (root-owned, minimal, installed once by hand) and the polkit rule restricting `deploy-agent` to allow-listed units; verify the full loop by hand (write to agent-owned unit dir → trigger reload → confirm systemd picks it up → confirm `StartUnit` on a non-allow-listed unit is denied)
5. Self-update via exit + `Restart=always`
6. Health checks + rollback
7. `deploy-ci`: bundle-build/sign/send/health-poll CLI reusing `deploy-common`; publish first tagged release with a prebuilt `x86_64-unknown-linux-gnu` binary asset
8. Reusable GitHub Actions workflow (`deploy-agent-workspace/.github/workflows/deploy.yml`) wrapping Tailscale join + pinned `deploy-ci` download + bundle/send/health-check; wire the first downstream project's short per-repo workflow to call it via pinned tag
9. `deploy-admin`: onboarding CLI (secret generation, project TOML templating, `/srv/apps/<project>` scaffolding, polkit snippet printing) — run once by hand for the first real project, under `sudo`
10. Hardening pass: replay window, body size limits, tar-slip sanitization
11. `/status` endpoint + retention/pruning + docs

## Reference: prior manual deploy script (being replaced)

The existing `send.sh` uses SSH multiplexing + scp to push binaries, systemd units (via `envsubst` templating), and certs, then remotely runs `systemctl stop/start` over SSH. This whole flow is what `deploy-agent` + the CI bundle/HMAC pipeline is replacing — SSH access from the deploying machine to the Pi should no longer be required for routine deploys once this is in place (kept only for bootstrapping and emergency manual access).
