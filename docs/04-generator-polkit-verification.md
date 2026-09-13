# Step 4: Generator Installation, Polkit Authorization, and the Full-Loop Verification

Per the build order, this step is narrower in code terms than Steps 1–3: the
generator script itself was already written in **3.4**, and `SystemdClient`
already makes the D-Bus calls it protects. What's missing is the two
root-owned, out-of-band artifacts that make those calls *actually
authorized and effective* on a real Pi — the generator's installation, and
the polkit rule — plus proving the whole loop end to end by hand, which is
the actual exit criterion from the handoff's Step 4.

This closes out open questions **#6** (exact polkit action IDs) and **#7**
(generator scope/format) from the handoff, and gives you the verification
procedure for the "still open" D-Bus/polkit-defaults item under the
privilege model section.

---

**Step 4 Open Design Decisions Resolved**

* **Generator source directory**: `/etc/deploy-agent/units/<project>/<unit>.service`, confirmed. Rationale: this is versioned config content (checked into a manifest, checksummed, agent-written but not agent-*state*), not runtime/cache state, so `/etc` fits the FHS convention better than `/var/lib`. It's also what Step 3's `sync_unit_files` and generator script already use — no code change needed, just locking it in as final rather than provisional.
* **Polkit action IDs**, confirmed against `systemd/org.freedesktop.systemd1.policy.in`:
  * `org.freedesktop.systemd1.manage-units` — gates `StartUnit`, `StopUnit`, `RestartUnit`, `ReloadUnit`, `TryRestartUnit`, `ReloadOrRestartUnit`, `ReloadOrTryRestartUnit`, `KillUnit`, `ResetFailedUnit`, and `SetUnitProperties`. Since systemd v244ish, the polkit action carries **two** details you can key a rule on: `unit` (the resolved primary unit name) and `verb` (the specific operation: `"start"`, `"stop"`, `"restart"`, `"reload"`, `"try-restart"`, `"reload-or-restart"`, `"reload-or-try-restart"`, `"kill"`, `"reset-failed"`, `"set-property"`). This means the rule can be tightened beyond just the unit name — see 4.2 below.
  * `org.freedesktop.systemd1.reload-daemon` — gates `Reload()` (the daemon-reload equivalent). Separate action, no `unit`/`verb` details, matches the handoff's note that this is coarser and granted once.
  * Default policy for `manage-units` without any matching rule is `auth_admin` (interactive admin auth required) — so the custom rule is what turns this from "always prompts/denies for a headless non-interactive service account" into "yes, for these specific units."

---

## 4.1 — Finalize and lock in the generator

Nothing in `deploy-agent/src/systemd.rs` or the generator script from 3.4
needs to change for the happy path. Two gaps are worth closing before you
treat this as "reviewed, rarely-changed" per the handoff's trust-boundary
framing:

**Gap 1 — no cleanup of removed units.** The generator only ever adds
symlinks; if a project's unit is removed from its manifest (or the whole
project is decommissioned), the stale symlink stays in
`/run/systemd/generator/` forever, and systemd keeps considering that unit
loaded until next reboot. Since `/run/systemd/generator/` is generator-owned
and repopulated by the generator on *every* reload, the fix is to make the
generator authoritative — remove any of *its own* previously-created
symlinks that no longer have a live source file, rather than only adding:

```bash
#!/bin/bash
# /etc/systemd/system-generators/deploy-agent-generator
set -euo pipefail

NORMAL_DIR="$1"
UNITS_DIR="/etc/deploy-agent/units"
MARKER_COMMENT="# deploy-agent-generator managed symlink"

mkdir -p "$NORMAL_DIR"

# Build the desired set: every *.service file under UNITS_DIR.
declare -A desired
if [ -d "$UNITS_DIR" ]; then
    while IFS= read -r unit_file; do
        filename=$(basename "$unit_file")
        desired["$filename"]="$unit_file"
    done < <(find "$UNITS_DIR" -type f -name "*.service")
fi

# Add/update symlinks for everything desired.
for filename in "${!desired[@]}"; do
    ln -sfn "${desired[$filename]}" "$NORMAL_DIR/$filename"
done

# Remove symlinks we own that no longer have a source — only touch
# symlinks that actually point back into UNITS_DIR, never anything else
# that might legitimately live in $NORMAL_DIR from another generator.
for existing in "$NORMAL_DIR"/*.service; do
    [ -e "$existing" ] || continue
    [ -L "$existing" ] || continue
    target=$(readlink -f "$existing" 2>/dev/null || true)
    filename=$(basename "$existing")
    case "$target" in
        "$UNITS_DIR"/*)
            if [ -z "${desired[$filename]:-}" ]; then
                rm -f "$existing"
            fi
            ;;
    esac
done
```

**Gap 2 — no visibility when the generator itself fails.** Generators run
as root very early in systemd's startup/reload sequence; a non-zero exit or
stderr output is captured by systemd and surfaced via
`systemctl status` / `journalctl` on modern systemd, but it's easy to miss.
`set -euo pipefail` (added above) at least ensures a broken `UNITS_DIR`
state fails loudly instead of silently producing a half-updated generator
directory.

Install it (root, one-time, out of band — same trust boundary as before,
this is never touched by the deploy pipeline):

```bash
sudo install -o root -g root -m 0755 \
  deploy-agent-generator /etc/systemd/system-generators/deploy-agent-generator
```

Sanity-check it runs without systemd invoking it, using the real generator
protocol (three directory args, in priority order — you only use `$1`
here, "normal" priority, which is correct: you're not trying to override
or be overridden by anything else in the unit search path):

```bash
sudo mkdir -p /tmp/gen-test/{early,normal,late}
sudo /etc/systemd/system-generators/deploy-agent-generator \
  /tmp/gen-test/normal /tmp/gen-test/early /tmp/gen-test/late
ls -la /tmp/gen-test/normal
```

---

## 4.2 — Write the polkit rule

The rule is a flat, human-reviewed file — per the handoff, `deploy-admin`
(Step 9) will eventually *print* the per-project snippet for a human to
paste in here, but it never writes this file itself. For now, hand-write
it covering your first real project plus the self-project unit:

`/etc/polkit-1/rules.d/49-deploy-agent.rules`:

```js
// Managed by hand today; deploy-admin (Step 9) will print the per-project
// snippet to paste in here, but never writes this file automatically —
// treat it as reviewed, rarely-changed, root-owned config.
polkit.addRule(function(action, subject) {
    if (subject.user != "deploy-agent") {
        return polkit.Result.NOT_HANDLED;
    }

    // Allow the coarse daemon-reload action unconditionally for this user —
    // it's not per-unit, and is needed whenever unit *content* changes.
    if (action.id == "org.freedesktop.systemd1.reload-daemon") {
        return polkit.Result.YES;
    }

    if (action.id == "org.freedesktop.systemd1.manage-units") {
        var allowedUnits = [
            "deploy-agent.service",
            "myproj-api.service",
            "myproj-worker.service"
        ];
        // Defense in depth beyond the unit allow-list: deploy-agent only
        // ever calls RestartUnit, so only authorize that verb even for
        // allow-listed units. Narrower than "any operation on this unit,"
        // which is what a unit-only check would grant.
        var allowedVerbs = ["restart", "reload-or-restart"];

        var unit = action.lookup("unit");
        var verb = action.lookup("verb");

        if (allowedUnits.indexOf(unit) != -1 && allowedVerbs.indexOf(verb) != -1) {
            return polkit.Result.YES;
        }
        return polkit.Result.NO;
    }

    return polkit.Result.NOT_HANDLED;
});
```

Two deliberate differences from the handoff's original sketch, worth
calling out since you're the one reviewing this file going forward:

- **Explicit `NOT_HANDLED` instead of falling through**, and an early
  `subject.user` guard — makes it unambiguous this rule only ever opines
  on the `deploy-agent` user and doesn't accidentally shadow other rules
  for other subjects/actions.
- **`verb` restricted to `restart`/`reload-or-restart`** — `RestartUnit`
  actually invokes the `"restart"` verb per systemd's own commit that
  introduced these details; `zbus`'s `restart_unit` proxy call you wrote in
  Step 3 maps to that. Keeping `StopUnit`/`KillUnit`/`SetUnitProperties`
  denied even for allow-listed units means a bug or compromise in
  `deploy-agent`'s Rust code can't be used to stop a service it was only
  ever supposed to restart — the OS-level gate is narrower than what the
  handoff's original sketch (unit-name-only) would have allowed.

polkit watches `/etc/polkit-1/rules.d/` for changes and reloads
automatically (no `daemon-reload` or service restart needed for the rule
itself — that's a systemd-side concept, not polkit's). Confirm pickup:

```bash
sudo journalctl -u polkit --since "1 minute ago"
# expect a line about reloading rules shortly after you save the file
```

---

## 4.3 — Confirm D-Bus bus-level access isn't a second gate

Before testing polkit denial/allow, rule out the other layer the handoff
flagged as "still open": whether the raw D-Bus policy (not polkit) permits
an unprivileged user to *send* `StartUnit`/`RestartUnit`/`Reload` to
`org.freedesktop.systemd1` at all. On some systemd vintages this was a
separate, more restrictive `busconfig` file
(`/usr/share/dbus-1/system.d/org.freedesktop.systemd1.conf` or similar)
that only allowlisted read-only methods for the default context, with
everything state-changing deferred to polkit only for callers it already
let through. Check what's actually shipped on your Pi's OS before assuming
either way:

```bash
find / -iname 'org.freedesktop.systemd1.conf' 2>/dev/null
cat <path-found-above>
```

If you find a `<policy context="default">` block that denies
`send_destination="org.freedesktop.systemd1"` by default and only
allowlists specific `send_member`s (none of which include
`RestartUnit`/`Reload`), you'll need a `<policy user="deploy-agent">` stanza
added there too — that's a second, root-owned file edit alongside the
polkit rule, not a `deploy-agent` code change. Recent systemd/dbus-broker
combinations on Debian-derived distros have trended toward deferring this
entirely to polkit and leaving the default D-Bus context permissive for
`org.freedesktop.systemd1`, but confirm rather than assume, since this is
exactly the kind of thing that varies by Raspberry Pi OS version — the
verification loop below (4.4) will tell you immediately which situation
you're in: a D-Bus-level deny surfaces as
`org.freedesktop.DBus.Error.AccessDenied` before your call ever reaches
polkit, whereas a polkit deny surfaces as
`org.freedesktop.PolicyKit1.Error.NotAuthorized`.

---

## 4.4 — Manual verification loop

This is the actual exit criterion: write to the agent-owned unit dir →
trigger reload → confirm systemd picks it up → confirm `RestartUnit` on an
allow-listed unit succeeds → confirm it's denied for a non-allow-listed
one.

**Set up a throwaway unit** so you're not restarting anything real yet:

```bash
sudo mkdir -p /etc/deploy-agent/units/myproj
sudo tee /etc/deploy-agent/units/myproj/myproj-api.service >/dev/null <<'EOF'
[Unit]
Description=verification target

[Service]
Type=oneshot
ExecStart=/bin/true
EOF
```

**Trigger a reload and confirm the generator picked it up.** Do this as
root first, to isolate "does the generator work" from "is polkit
authorizing deploy-agent" — you'll redo the reload as the `deploy-agent`
user once this part is confirmed:

```bash
sudo systemctl daemon-reload
systemctl status myproj-api.service
readlink -f /run/systemd/generator/myproj-api.service
# expect it to resolve back to /etc/deploy-agent/units/myproj/myproj-api.service
```

**Now exercise the actual D-Bus calls as the `deploy-agent` user**, not
root, since that's the identity polkit is evaluating. If the system account
has no login shell (likely, since it shouldn't), use `runuser`/`machinectl
shell` rather than `su -`:

```bash
sudo runuser -u deploy-agent -- \
  busctl call --system org.freedesktop.systemd1 /org/freedesktop/systemd1 \
  org.freedesktop.systemd1.Manager Reload
```

Expect this to succeed silently (empty reply) — it's gated only by
`reload-daemon`, unconditionally allowed above.

```bash
sudo runuser -u deploy-agent -- \
  busctl call --system org.freedesktop.systemd1 /org/freedesktop/systemd1 \
  org.freedesktop.systemd1.Manager RestartUnit ss "myproj-api.service" "replace"
```

Expect this to succeed and print an object path (the job). This is the
positive case: allow-listed unit, allowed verb.

```bash
sudo runuser -u deploy-agent -- \
  busctl call --system org.freedesktop.systemd1 /org/freedesktop/systemd1 \
  org.freedesktop.systemd1.Manager RestartUnit ss "ssh.service" "replace"
```

Expect this to fail with `org.freedesktop.PolicyKit1.Error.NotAuthorized`
(not a D-Bus `AccessDenied`, per 4.3's distinction — if you get
`AccessDenied` instead, your D-Bus bus policy is the layer denying it, and
you have the 4.3 follow-up to do). This is the negative case: correctly
rejects a unit outside the allow-list, on a unit that very much exists and
would otherwise be a plausible target.

Also confirm the verb restriction actually bites, not just the unit
allow-list — try `StopUnit` on the *allow-listed* unit:

```bash
sudo runuser -u deploy-agent -- \
  busctl call --system org.freedesktop.systemd1 /org/freedesktop/systemd1 \
  org.freedesktop.systemd1.Manager StopUnit ss "myproj-api.service" "replace"
```

Expect `NotAuthorized` here too — this confirms the `verb` check in 4.2 is
doing real work, not just the `unit` check.

**Finally, run it through the actual agent**, not `busctl`, to confirm
`SystemdClient::restart_unit_and_await` behaves correctly against a real
`NotAuthorized` denial rather than just a synthetic `busctl` call — deploy
`myproj` for real (per Step 2's signed-bundle flow) with `ssh.service`
temporarily added to its manifest's `units` but *not* to
`allowed_units` in `myproj.toml`. Confirm the agent's own allow-list check
(Step 3's `if project_config.allowed_units.contains(...)`) already skips it
before ever reaching D-Bus — the polkit denial you just proved is the
defense-in-depth layer that should ideally never fire in practice, only
when that first check is missing or wrong.

---

## 4.5 — Surface `NotAuthorized` distinctly in the agent's own error path

Right now, `restart_unit_and_await`'s `.context(...)` wrapping turns *any*
D-Bus failure — connection refused, unit doesn't exist, polkit denial —
into the same generic
`"D-Bus call RestartUnit for '{unit_name}' failed"` message. Since 4.4 just
proved polkit denial is a real, reachable failure mode (not just
theoretical defense-in-depth), it's worth distinguishing it in the logs so
a denied-by-policy failure doesn't get mistaken for a transient D-Bus
hiccup during on-call triage:

```rust
    pub async fn restart_unit_and_await(&self, unit_name: &str) -> Result<()> {
        let proxy = SystemdManagerProxy::new(&self.conn).await?;
        let mut job_removed_stream = proxy.receive_job_removed().await?;

        let job_path = match proxy.restart_unit(unit_name, "replace").await {
            Ok(path) => path,
            Err(zbus::Error::MethodError(ref name, _, _))
                if name.as_str() == "org.freedesktop.PolicyKit1.Error.NotAuthorized" =>
            {
                bail!(
                    "polkit denied RestartUnit for '{unit_name}' — unit missing from \
                     /etc/polkit-1/rules.d allow-list, or project config and polkit \
                     rule have drifted out of sync"
                );
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("D-Bus call RestartUnit for '{unit_name}' failed")
                });
            }
        };

        tracing::info!("initiated restart for unit '{unit_name}', job path: {job_path}");

        while let Some(signal) = job_removed_stream.next().await {
            let args = signal.args().context("parsing JobRemoved signal args")?;
            if args.job == job_path {
                if args.result == "done" {
                    tracing::info!("job for unit '{unit_name}' completed successfully (result=done)");
                    return Ok(());
                } else {
                    bail!("job for unit '{unit_name}' failed with result: '{}'", args.result);
                }
            }
        }

        bail!("JobRemoved stream ended before unit '{unit_name}' completed");
    }
```

This is a small, additive change — it doesn't touch `main.rs`'s call site,
since the `Result<()>` shape is unchanged, just the error message content.

---

## 4.6 — Verification & Step Exit Criteria

Before proceeding to **Step 5: Self-Update via Exit + `Restart=always`**
(which, per your note, you've effectively already started — worth
double-checking the below still holds now that self-update and pruning
exist):

* [ ] Generator installed at `/etc/systemd/system-generators/deploy-agent-generator`, `root:root`, `0755`
* [ ] Generator removes stale symlinks for units no longer present under `/etc/deploy-agent/units/`, confirmed by deleting a unit file and re-running the generator by hand
* [ ] Polkit rule installed at `/etc/polkit-1/rules.d/49-deploy-agent.rules`; `journalctl -u polkit` shows it was picked up without a manual reload
* [ ] Confirmed (4.3) whether your Pi's OS has a separate D-Bus bus-level policy file gating `org.freedesktop.systemd1`, and if so, added the corresponding `<policy user="deploy-agent">` stanza
* [ ] As the `deploy-agent` user via `busctl`: `Reload()` succeeds, `RestartUnit` succeeds for an allow-listed unit, `RestartUnit`/`StopUnit` both fail with `NotAuthorized` for a non-allow-listed unit
* [ ] A real deploy through the agent, with a unit present in the manifest but absent from `allowed_units`, is rejected by the agent's own check before it reaches D-Bus at all (confirms defense-in-depth ordering, not just that polkit alone would have caught it)
* [ ] `restart_unit_and_await` surfaces a distinct, actionable message on polkit denial rather than the generic D-Bus failure message

Once these hold, the systemd-interaction surface described in the handoff's
privilege model is fully proven end to end: `deploy-agent` never needs root
or `sudo`, and every unit it can touch is enforced twice — once in Rust
against `allowed_units`, once by the OS via polkit — with the two now
verified to actually agree rather than just both existing in the codebase.

Since you've already got self-update and pruning working ahead of
schedule, want **Step 6: Health Checks + Rollback** next instead of a
strict re-run of Step 5, or do you want Step 5 written up properly first
so there's a matching doc for what you built by hand?
