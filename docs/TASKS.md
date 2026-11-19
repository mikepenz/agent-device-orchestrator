# devq: task document

Source plan: `~/.claude/plans/pasted-content-id-6485-you-are-zippy-pillow.md` (approved).
Summary: `devq` is a single-machine Rust CLI. The coordinator is a SQLite file at `$DEVQ_HOME/devq.db`, defaulting to `~/.devq`. It hands out exclusive device slots through a FIFO queue and shows them on a live web page.

Target shared resource for verification: the running emulator **`emulator-5554`**, the only device attached.

## Pipeline per phase

```
implement (Sonnet 5.5)  →  validate (Sonnet 5.5, code review against this doc)  →  test (Haiku 5.5, runs the checks below)
          ↑______________________ fix loop if validate or test fails ______________________|
```

A phase is done only when every acceptance check below passes and is reported with its command output.

## Contracts (fixed, every phase relies on these)

### CLI

- Shared slot flags: `--agent` (env `DEVQ_AGENT`, required), `--purpose` (required), `--description` (default empty), `--expect <duration>` (default 10m), `--max <duration>` (default 30m, must be ≥ expect), `--device <serial>` (optional).
- `devq request [slot flags] [--wait <dur>] [--ticket <id>] [--json]`
  - Exit 0 when granted. Stdout is `<slot_id> <serial>`, or `{"slot":id,"device":serial}` with `--json`.
  - Exit 75 when `--wait` expires. Stdout is `ticket <id>`. Re-running with `--ticket <id>` resumes in the same queue position.
- `devq release <id>`: exit 0. Cancels the slot if waiting, releases it if active. Exit 1 if the id is unknown or already done.
- `devq run [slot flags] -- <cmd...>`: acquires a device and sets `ANDROID_SERIAL`, `DEVQ_DEVICE`, and `DEVQ_SLOT` for the child. Exits with the child's exit code. Forwards SIGINT/SIGTERM to the child. When `--max` elapses it sends SIGTERM, then SIGKILL 5s later.
- `devq status [--json]`, `devq devices [--json]`.
- `devq live [--port 7878] [--open]`: serves on 127.0.0.1 only.

### `GET /api/state?window=<secs, default 3600>`

```jsonc
{
  "now": 1763560000000,                 // unix ms
  "since": 1763556400000,
  "devices": [{
    "serial": "emulator-5554", "kind": "android", "model": "sdk_gphone64_arm64", "state": "online",
    "slot": null | { "id": 7, "agent": "claude-a", "purpose": "instrumented tests", "description": "...",
                     "command": "./gradlew cAT" | null, "granted_at": 0, "expect_secs": 600, "max_secs": 1800, "deadline": 0 }
  }],
  "queue": [{ "id": 8, "agent": "...", "purpose": "...", "description": "...", "want_device": null, "requested_at": 0, "expect_secs": 600 }],
  "history": [{ "id": 6, "device": "emulator-5554", "agent": "...", "purpose": "...", "description": "...",
                "command": "./gradlew cAT" | null, "want_device": null, "requested_at": 0, "granted_at": 0,
                "released_at": 0 | null, "end_reason": "released|timeout|orphaned|cancelled|abandoned" | null,
                "exit_code": 0 | null, "expect_secs": 600, "max_secs": 1800 }],
                // slots overlapping the window (granted before `since` but released inside it count), active ones included
  "stats": { "grants": 0, "wait_p50_ms": 0, "wait_p95_ms": 0, "hold_mean_ms": 0, "expect_ratio": 1.0,
             "timeouts": 0, "orphaned": 0, "abandoned": 0, "queued": 0 },
  "per_device": [{ "serial": "...", "grants": 0, "busy_ms": 0, "utilisation": 0.0 }],
  "per_agent":  [{ "agent": "...", "grants": 0, "busy_ms": 0, "wait_mean_ms": 0, "timeouts": 0 }]
}
```

## Phase 1: core (`Cargo.toml`, `src/db.rs`, `src/pool.rs`, a minimal `src/main.rs`)

Deliverables: the schema (`devices`, `slots`) and `tick(conn, now)`. `tick` reaps timeouts, orphaned pids, and abandoned waiters (no heartbeat for 30s), then allocates FIFO, with pinned requests skipped rather than blocking. It also covers the `adb devices -l` parser, the config at `$DEVQ_HOME/config.toml` (`include`/`exclude` globs, `static` list), the stats queries for the `/api/state` shape, and WAL mode with `busy_timeout`.

| # | Acceptance check | Measure |
|---|---|---|
| 1.1 | `cargo build --release` | 0 warnings, 0 errors |
| 1.2 | `cargo test` | ≥ 8 tests pass. Required: FIFO order, pinned request does not block others, timeout reap, abandoned reap, orphan reap (dead pid), release, cancel, adb parser (incl. `offline`/`unauthorized` lines), glob filter |
| 1.3 | `cargo clippy -- -D warnings` | clean |

## Phase 2: CLI (`src/main.rs`)

Deliverables: every command in the CLI contract.

| # | Acceptance check (`DEVQ_HOME` = fresh temp dir, real `emulator-5554`) | Measure |
|---|---|---|
| 2.1 | `devq devices` | lists `emulator-5554`, state `online`, kind `android` |
| 2.2 | A: `devq request --agent a --purpose p` → B: `devq request --agent b --purpose p --wait 2s` | A exits 0 with `emulator-5554`; B exits **75** within 2–3s |
| 2.3 | `devq release <A>` then B: `devq request ... --ticket <B>` | B granted within 1s, same slot id |
| 2.4 | Two concurrent `devq run --agent x --purpose t -- sh -c 'adb shell getprop ro.product.model; sleep 4'` | Both print `sdk_gphone64_arm64`. Their hold intervals do not overlap (check `granted_at`/`released_at` in `status --json` or the db). Total wall time ≥ 8s |
| 2.5 | `devq run ... -- sh -c 'echo $ANDROID_SERIAL; exit 3'` | prints `emulator-5554`, exit code **3** |
| 2.6 | `devq request --expect 1s --max 3s` and never release; second request waits | second granted within 3–5s; first slot `end_reason=timeout` |
| 2.7 | `devq run ... -- sleep 60 &`, then `kill -9` the devq pid; a waiter is queued | waiter granted within 2s of the kill; slot `end_reason=orphaned` |
| 2.8 | `devq run --max 3s -- sleep 60` | child killed, devq exits non-zero in ≤ 9s, `end_reason=timeout` |
| 2.9 | Config `exclude = ["emulator-5554"]`, `static = ["rig-a"]` | `devices` shows only `rig-a`; `run` on it works |

## Phase D: design (runs in parallel with Phases 1–2)

Deliverables:
- A published **live Artifact with 2–3 proposals** for the dashboard, switchable in the page, animated with a mock data simulator using the `/api/state` shape above. Used for fine-tuning.
- `design/live.html`: the recommended proposal as one self-contained file with no CDN, vanilla JS, and inline SVG. It polls `/api/state` every 1s and falls back to its built-in simulator when the API is unreachable (`?mock=1` forces the simulator).
- `design/NOTES.md`: tokens, layout, motion rules, and the reasoning behind the choice (≤ 1 page).

| # | Acceptance check | Measure |
|---|---|---|
| D.1 | Artifact URL returned and opens | proposals switchable; simulator shows grant, release, queue moves, timeout |
| D.2 | Covers every plan element | device cards (holder, purpose, description, command, elapsed vs expected bar, overrun, deadline), queue, timeline 1h/6h/24h, stat tiles, per-device and per-agent tables |
| D.3 | Motion | state changes animate (card status, bar growth, queue enter/leave); respects `prefers-reduced-motion` |
| D.4 | Themes and layout | light and dark both legible; no horizontal scroll at 375px width |
| D.5 | No layout jump (user feedback) | cumulative layout shift ≈ 0 over 60s of the simulator at high speed (PerformanceObserver `layout-shift`); queue changes never move the content below it |
| D.6 | Run log (user feedback) | past and active slots, newest first, filterable by agent, device and outcome, searchable; clicking a row or timeline bar shows every field plus a requested→granted→released mini timeline |
| D.7 | Visual pass (user feedback) | `frontend-design` + `unslop` applied; minimal and modern, no generic "AI dashboard" look (gradients, glows, pill overload, heavy cards) |

## Phase 3: live server (`src/live.rs`, `src/live.html` ← `design/live.html`)

| # | Acceptance check | Measure |
|---|---|---|
| 3.1 | `devq live --port 7878` + `curl /api/state` | valid JSON matching the contract keys; `?window=60` changes `since` |
| 3.2 | `curl /` | serves the page (contains `<title>`), 0 external URLs |
| 3.3 | Run scenario 2.4 while the page is open in Chrome | screenshot shows the device busy with holder and a waiting queue entry, then a released state; timeline bars appear |
| 3.4 | Bound address | `lsof -i :7878` shows 127.0.0.1 only |

## Phase 4: wrap-up

`README.md` with usage and a snippet to paste into agent instructions. A final full run of 1.x, 2.x, and 3.x.

## Phase 5: agent skill + real multi-agent scenario

Deliverables: `.claude/skills/devq/SKILL.md` (how agents use devq), and a live run in which a Sonnet lead spawns 3 Sonnet worker agents. Each worker loads the skill and does several real adb tasks of different lengths on `emulator-5554`, coordinated only through devq. A human watches `devq live`.

| # | Acceptance check | Measure |
|---|---|---|
| 5.1 | Skill commands work as written | each documented flow ran against the emulator (run, request→release, ticket resume) |
| 5.2 | Workers follow the skill | every worker's adb call happens inside a slot and uses `-s $ANDROID_SERIAL`; no slot left active at the end |
| 5.3 | Contention is real | at least one exit-75 ticket resume, queue depth ≥ 2 at some point, and every holder ran without overlap (zero overlapping `granted_at..released_at` intervals) |
| 5.4 | Dashboard reflects it | lead's report includes per-agent wait/hold stats from `/api/state` and a screenshot of the live page mid-run |

## Phase 6: `devq skills` command

Status:
- **Test PASS**: steps 1–9 ran against a temp HOME. Discovery was confirmed with `claude -p` in a temp project.
- **DONE.** Retest PASS after the fixes, including the HOME-unset, non-UTF-8, directory, dangling-symlink, quoted `~` and source-copy-protection cases. Re-review PASS. A content-only copy of the source can now be overwritten with `--force`; the repo's own source path stays refused even with `--force`.
- **Review FAIL** (resolved), mostly on data safety. Fixes:
  - empty HOME writes relative to the current dir
  - a non-UTF-8 file is treated as missing and overwritten
  - the repo's own source copy can be clobbered
  - `--dir '~'` is not expanded
  - default `update` should ignore foreign copies
- The skill is being improved separately with skill-creator, at the user's request, including trigger evals.

Modelled on `android skills` (`add | list | update | remove`; `find` is skipped while there is only one skill).

- The skill ships inside the binary: `include_str!("../.claude/skills/devq/SKILL.md")`. The installed version therefore always matches the CLI version.
- On install, these keys are added to the frontmatter: `metadata: { devq-version: <crate version>, devq-sha: <sha256 of body> }`. They identify devq-owned copies and detect edits made by hand.
- Targets:
  - global `~/.claude/skills/devq/` (default)
  - `--project` → `./.claude/skills/devq/`
  - `--dir <path>` for other agent tools (Codex and others), without guessing their layouts

| Command | Behaviour |
|---|---|
| `devq skills list` | Bundled skills, plus the state of each target: `not installed` / `installed vX` / `outdated (vX → vY)` / `modified locally` |
| `devq skills add [devq] [--project\|--dir P] [--force]` | Writes `SKILL.md`. Refuses to overwrite a locally modified copy unless `--force` is given. |
| `devq skills update [--project\|--dir P]` | Rewrites outdated copies that have not been modified. Modified copies are reported and skipped. |
| `devq skills remove [--project\|--dir P] [--force]` | Deletes the copy only if it carries the devq marker and is unmodified, unless `--force` is given. |

Acceptance:
- Unit tests cover the cycle add → list → modify → update skipped → `--force` → remove, run against a temp HOME.
- e2e: after `add`, a fresh `claude` session lists the `devq` skill.
- `add` prints a warning when the `devq` binary is not on PATH.

## Phase 7: start it like a Mac app

Status:
- **Review PASS** and **test PASS**.
  - Port 7892 test: plist valid per `plutil`, the 7 routes return correct content types, a bad Host gets 403.
  - KeepAlive respawned the server 1s after `kill -9`.
  - In Chrome the service worker was activated and the manifest was valid.
  - Uninstall is idempotent and nothing is left behind.
- **DONE.** All follow-ups are fixed and verified on port 7891: reinstall while loaded, `restart` changes the pid, `kill -9` respawns, uninstall twice, and no LaunchAgent is left behind (orchestrator confirmed). 29 tests. The follow-ups were:
  - wait for bootout before bootstrap on reinstall
  - stop canonicalizing the exe path (Homebrew upgrades)
  - add `service restart`
  - absolute paths in the plist
  - probe the port after install
  - service worker caches only `r.ok` responses
  - gate service.rs for Windows

The page only works while `devq live` is running. The aim is therefore to (1) keep the server running and (2) give the page its own window and Dock icon.

1. `devq service install | uninstall | status`:
   - Writes `~/Library/LaunchAgents/dev.devq.live.plist` (RunAtLoad, KeepAlive, fixed port 7878, logs in `~/.devq/live.log`) and runs `launchctl bootstrap gui/$UID …`.
   - On Linux: a systemd `--user` unit (later).
   - About 60 lines in total.
2. PWA:
   - `/manifest.webmanifest`: name, `display: standalone`, theme colours, and a 512px SVG/PNG icon embedded in the binary.
   - A `<link rel="manifest">` in the page.
   - A minimal service worker that only caches the app shell. Data is always fetched live.
   - Installing it gives a Dock app with its own window: Chrome via "Install devq live…", Safari via "Add to Dock". The Host check already allows `127.0.0.1:7878`.
   - About 40 lines plus an icon.
3. Not doing: a Tauri/wry native wrapper. It brings a large dependency tree, code signing and notarization, and gives nothing beyond what a PWA window already provides. Revisit if menu-bar status or native notifications become needed.

Acceptance: after `devq service install`, a reboot or logout keeps `/api/state` reachable. Chrome reports the page as installable (Application → Manifest shows no errors). The installed app opens in a standalone window.

## Phase 8: Linux / Windows portability

Status: **DONE.**
- **Code split:** all platform code now lives in `src/sys.rs`. `libc` and `signal-hook` are unix-only dependencies, `ctrlc` and `windows-sys` are windows-only.
- **macOS:** 33 tests pass, and `e2e.sh` and `e2e-live.sh` print ALL PASS.
- **Linux:** strict Docker build with `-D warnings` has 0 warnings, 31 tests pass, and the smoke test is ALL PASS. The smoke test now asserts the orphan count, detects missing tools, and checks empty `HOME` and `DEVQ_HOME`.
- **Windows:** `check`, `clippy` and `check --tests` on the gnu target are clean. The runtime is untested.
- **Review fixes (1st review FAIL, now resolved):**
  - Windows always force-kills, with an honest "no grace" comment.
  - The pid guard refuses pids ≤ 4 and our own pid.
  - `ACCESS_DENIED` is treated as alive.
  - An empty `DEVQ_HOME` falls back to `HOME`.
  - Test gates added.
  - The README lists the Windows limits.

Audit results:
- **Linux works as-is.** In a Docker `rust:1` aarch64 container: build with 0 warnings, 16/16 tests, and a smoke test without adb passed. The smoke test covered a static device, run with exit code passthrough, request/release, live with the Host check, and orphan reap.
- **Windows does not compile.** There are 10 errors, all from unix process APIs:
  - `std::os::unix` and `process_group`
  - `libc::kill/killpg/getpgrp/SIGKILL`
  - `ExitStatusExt::signal`
- Windows also has silent bugs:
  - `$HOME` is unset there, so the home directory resolves relative to the current directory.
  - `--open` calls `xdg-open`.

Plan: one `src/sys.rs` with `#[cfg(unix)]`/`#[cfg(windows)]` versions of `pid_alive`, `spawn_grouped`, `kill_tree(id, force)`, `exit_code` and `install_signals`.

| Area | Windows implementation |
|---|---|
| Process group | `CREATE_NEW_PROCESS_GROUP`; store the pid in `child_pgid`; kill with `taskkill /PID n /T`, then `/F` after the 5s grace. A Job Object is not usable here because other processes reap orphans by id. |
| Liveness | `OpenProcess` + `GetExitCodeProcess == STILL_ACTIVE` (`windows-sys`) |
| Signals | `ctrlc` crate (termination feature) sets the same flag; SIGTERM handling stays unix-only |
| Small fixes | Exit code via `code().unwrap_or(1)`; `--open` via `cmd /C start "" url`; home from `HOME`, falling back to `USERPROFILE` |
| Dependencies | `windows-sys` and `ctrlc` under `cfg(windows)`; `libc` becomes unix-only |
| Tests | Unix-only tests are gated with `#[cfg(unix)]` |

- Estimated size: about 80 lines.
- Linux gets two small changes: fall back to printing the URL when `xdg-open` is missing, and fail loudly when `HOME` is unset.

Acceptance:
- macOS: e2e is unchanged and green.
- Linux: the Docker build, tests and smoke script pass. The smoke script moves into `scripts/smoke-linux.sh`.
- Windows: `cargo check --target x86_64-pc-windows-gnu` has 0 errors. Run it with `CC`/`AR` stubbed, or install mingw only if the user OKs it. Actual Windows runtime is untested; the README says so.

## Skill improvement (skill-creator pass, done)

- **Description:** now starts with "Gate for any Android device or emulator work". It adds the device verbs and the intent, and states when NOT to use it.
- **Body:** gains a "what counts as device work" section, `devq devices` instead of `adb devices`, `$ANDROID_SERIAL` quoting, and `logcat -d`. 84 lines; no reference files, because `devq skills add` ships one file.
- **Trigger evals:** `claude -p` in a temp project, 2 runs each.
  - Substantive device tasks: **16/16 trigger**.
  - Near misses (ViewModel test, Azure DevOps, explain adb, manifest review, …): **0/12 false triggers**.
  - Terse one-liners ("install the debug apk"): **0/20**, before and after, and even with "MANDATORY" wording.

  Description-based triggering alone cannot catch terse commands, which is the case for the Phase 9 hook and the CLAUDE.md/AGENTS.md snippet.

## Phase 9: optional enforcement hook

Status: **DONE.**
- **First review: FAIL**, now resolved. Fixes:
  - `is_ours` deleted look-alike user hooks; it now does an exact token match.
  - Bypasses closed: `bash -lc`, `sudo`, `doas`, `watch`, `env -u`/`-C`/`-S`, `eval`, `{ }`/`if`/`do` keywords, `connectedCheck`, `adb.exe`.
  - `adb --help` was a false positive.
  - Per-agent `-s` is enforced when `DEVQ_AGENT` is set.
  - The Codex `/hooks` trust note is printed.
  - `preserve_order` is enabled, and the `blocked` table is pruned to 30 days.
- **Test: PASS.**
  - Slot-based allow and deny checked on emulator-5554.
  - stdin, fail-open and `--strict`.
  - Blocked records appear in `/api/state`.
  - About 17 ms per call.
  - The installer merge, idempotency, exact uninstall and invalid-JSON refusal all work.
- **Live run:** `claude -p` without the skill got blocked, then retried via `devq run`; with the skill installed it used devq directly.
- **Orchestrator re-check after the fixes:** 35 tests pass, every review bypass now gives deny, and the false positives give allow.
- **Accepted limits** (`ponytail:` notes):
  - scripts or `python -c` that call adb internally
  - `$(which adb)` and `$ADB`
  - gradle task abbreviations such as `cAT`
  - any slot counts when `DEVQ_AGENT` is unset
  - the Codex and Gemini formats are unverified against a live CLI

Goal: an agent that skips the skill gets blocked from raw device commands, and the block message tells it how to use devq. This is opt-in, and it is a guardrail, not a security boundary. All three agent CLIs document their hooks as guardrails.

Findings (Claude Code docs, plus the Codex and Gemini hook references):
- **Hooks in all three CLIs:**
  - Claude Code `PreToolUse` gets `tool_input.command`, plus `agent_type` inside subagents.
  - Codex `PreToolUse` is near-identical.
  - Gemini `BeforeTool` exists.
  - **All three treat `exit 2` + stderr as "deny, show this reason to the model"**, so one hook binary covers every CLI.
- **Claude Code permission deny rules** (`Bash(adb *)`) split compound commands (`&&`, `|`, `;`, subshells). They miss `sh -c '…'`, absolute paths and scripts, so they make a static backstop only.
- **Plugins:** a Claude Code plugin can ship the skill and the hook together, with one install.

Design:
1. **`devq hook`** reads the hook JSON from stdin.
   - It splits the command into subcommands: `&&`, `||`, `;`, `|`, `&`, newlines, `$(…)`, and `sh -c`/`bash -c` strings, recursively. It strips env assignments and wrappers (`timeout`, `nice`, `env`, …).
   - **Flagged:**
     - any `adb` or `*/adb` call except `adb devices`/`adb version`/`adb help`
     - `am instrument`
     - `gradlew … connected*Test`
     - `emulator -avd`
   - **Allowed:**
     - the whole command runs under `devq run … --`
     - or the adb call targets `-s <serial>` / `ANDROID_SERIAL=<serial>` and that serial has an active slot in the db
     - `devq` itself
   - **Deny path:** exit 2, with stderr saying "No devq slot for this device command. Use the `devq` skill: `devq run --agent … --purpose … -- <cmd>`".
   - **Failure policy:** fail-open on internal errors by default; `--strict` makes it fail-closed.
   - **Logging:** each block is recorded as a `blocked` event and shown on the dashboard, which reveals agents that skip the skill.
2. **`devq hook install [--project|--global] [--target claude|codex|gemini] [--deny-rules]`**
   - Merges an idempotent hook entry into the right settings file (`.claude/settings.json`, `~/.codex/hooks.json`, or Gemini `settings.json`). It uses serde_json and keeps every other key untouched.
   - `--deny-rules` also adds the static Claude Code backstop rules.
   - `devq hook uninstall` removes exactly what was added.
   - `devq hook test "<cmd>"` prints the verdict, for debugging.
3. **Later, if needed:**
   - a Claude Code plugin that bundles skill + hook (`devq plugin export`)
   - managed settings for org-wide mandatory enforcement
   - an `adb` PATH shim for tools without hooks

Acceptance:
- **Unit tests:** a table of about 30 commands with their expected allow/deny verdicts, including `sh -c`, pipes, `cd x && adb`, absolute adb paths, gradlew connected tests and `devq run` wrapping.
- **e2e:**
  - With the hook installed in a temp project, `claude -p` with "run adb shell getprop on the emulator" either gets blocked and then retries via devq, or uses devq directly.
  - The dashboard shows any blocked attempt.

## Execution order (user approved on 2026-11-19: rename to `devq`, then build phases 6, 7 and 8)

1. Rename `ado` → `devq`, together with the PWA icon and manifest design (in parallel, no shared files).
2. Phase 6 `devq skills`: implement → review → test.
3. Phase 7 `devq service` + PWA, which reuses the icon and manifest: implement → review → test (Chrome install check).
4. Phase 8 portability fixes from the audit: implement → review → test (Linux in Docker, `cargo check` for Windows).
5. Final regression, then restart the user's dashboard as `devq live` (or as the launchd service).

## Status

| Phase | State | Notes |
|---|---|---|
| 1 | done | test PASS. Review FAIL → 10 fixes applied in Phase 2 (end_slot timeout, adb-failure guard, window clamp, straddle utilisation, pid guard, parser whitelist, deny_unknown_fields), 14 tests |
| D | validating | Design artifact (A/B/C + tune panel). Recommended A "control room" → `design/live.html`. Source: `design/src/src.html` + `build.py`. Chrome test PASS (D.1–D.4). Review FAIL → fixed (v3). User feedback round (v4): CLS 0.18 → 0.0006, run log + detail drawer, single minimal design (A/B/C toggle removed), timeline kept. **Re-review PASS; final Chrome test PASS** (CLS 0.00006, run log filters/search/drawer, 375px, themes). Final polish round (stats hierarchy, a11y, robustness) running |
| 2 | validating | e2e ALL PASS; Haiku test PASS (+ manual lock/free checks). Review FAIL: (A) waiter's tick SIGKILLs immediately → no 5s TERM grace; (B) grandchildren survive leader exit → device re-granted under them. +10 risks (slot leak pre-holder, unpolled tickets granted, adb timeout, ...) and e2e false-PASS spots. Fix round 2 done: 16 tests, e2e 2.1–2.11 ALL PASS. **Re-review PASS, re-test PASS** (e2e 2× stable + manual a–i). Follow-ups queued: Sigs only for request/run, LIVE_MS→8s, bounded adb read, migration race, cap --max |
| 3 | validating | `src/live.rs` (embeds `design/live.html`). Review PASS → fixes applied (Host check vs DNS rebinding → 403, window ≤ 7d, config per request) + Phase 2 follow-ups. e2e.sh + e2e-live.sh ALL PASS, 16 tests, 0 warnings (verified by orchestrator). **3.3 PASS** in Chrome against emulator-5554 (busy holder + command, queue, release → handoff, run log exit 0, device free after release) |
| 5 | **PASS** (5.1–5.4) | Real run 14:54–14:59: 10 slots, 3 agents, 0 overlaps, 0 leftovers, 3 ticket resumes, max queue 2, codex-perf overran its 40s estimate (78s) without a kill. Worker feedback → skill fix running (acquire loop in one shell call, check rc before parsing — one worker parsed `ticket 7` as a serial). Also: open() fast path + WAL check; "—" for empty stats |
| 4 | done (follow-ups: open() fast path, verify WAL mode, stress script greps stderr) | README + full clap help done. **Final regression PASS** (0 warnings, clippy, 16 tests, e2e 2.1–2.11, e2e-live, Chrome live/dark/375px). Found: "database is locked" when two `ado` start at once on a fresh DEVQ_HOME → fixing in `db::open` + e2e 2.12 |
