---
name: devq
description: Gate for any Android device or emulator work. Use before running adb (shell, install, logcat, screencap, getprop), installing or launching an APK, taking screenshots or screen recordings, running instrumented or connected tests (connectedAndroidTest, connectedDebugAndroidTest, am instrument), checking the model or state of an attached phone or emulator (emulator-5554), or using any other shared resource managed by devq. Devices are shared between agents, so use this skill whenever a task would touch a real or emulated device, even if the user never mentions devq, to get an exclusive slot first. Not needed for host-only work such as JVM unit tests, builds, or explaining what adb is.
---

# Using shared devices with devq

Devices are shared with other agents. `devq` queues requests and gives each holder one device at a time.

## The rule

Never touch a device without holding a slot. The lock is advisory, so nothing stops a stray `adb` call, and that call can wreck another agent's test run. Anything that talks to a device counts: install, shell, logcat, screencap, getprop, instrumented tests. Host-only work (builds, JVM unit tests) needs no slot. To list devices, use `devq devices`, not `adb devices`.

Target the granted serial every time: `adb -s "$ANDROID_SERIAL" ...`. Never rely on "whatever is attached". Inside `devq run`, `ANDROID_SERIAL` is already set. After `devq request`, use the serial it printed.

## Pick an agent name

Pass `--agent <name>` or export `DEVQ_AGENT`. Use one stable name per agent, such as its role (`checkout-tests`, `ui-reviewer`). Humans read it on the dashboard.

## One command: `devq run`

```sh
devq run --agent "$NAME" --purpose "<why>" --description "<what the command does>" \
  --expect 5m --max 15m -- ./gradlew connectedDebugAndroidTest
```

`devq run` waits for a device, runs the command with `ANDROID_SERIAL`, `DEVQ_DEVICE` and `DEVQ_SLOT` set, then releases the slot. Stdin is `/dev/null`.

- `--expect` is an honest estimate of the hold time. It is advisory: it only drives the dashboard's overrun display and stats. Nothing is enforced. Default 10m.
- `--max` is a hard kill: SIGTERM to the command's process group, SIGKILL 5s later. Set it to roughly 2-3x `--expect`. Too tight kills a healthy run. Default 30m. It counts from the grant, so time spent queued does not count. `--max` must be >= `--expect`, so lower `--expect` too when you set a short `--max`.
- Exit code: the child's own code is passed through. 127 means the command could not start. 124 means `--max` killed it. 128+signal means it was signalled.
- A one-off adb call works as `devq run ... -- adb shell getprop ro.product.model`, because adb reads `ANDROID_SERIAL` itself. If the command needs the serial in its arguments, quote it so the child expands it, not your shell: `-- sh -c 'adb -s "$ANDROID_SERIAL" exec-out screencap -p > shot.png'`.
- `devq run` has no `--wait`. If every device is busy it blocks until one frees up. If your tool call may time out first, use the request block below.

## Several steps: `devq request`

Run the whole block in ONE Bash call and raise the tool timeout (for example 600000 ms). The loop re-polls the ticket within milliseconds. Separate tool calls can take longer than the 8s window, and then the queue skips you.

```sh
(
  out=$(devq request --agent "$NAME" --purpose "<why>" --description "<what>" \
          --expect 5m --max 15m --wait 30s); rc=$?
  while [ $rc -eq 75 ]; do                      # 75: still queued, out is "ticket <id>"
    out=$(devq request --ticket "${out#ticket }" --wait 30s); rc=$?
  done
  [ $rc -eq 0 ] || { echo "devq request failed ($rc): $out" >&2; exit $rc; }
  read -r SLOT SERIAL <<<"$out"                 # only after rc is 0
  trap 'devq release "$SLOT"' EXIT               # armed after the grant, fires on any exit
  adb -s "$SERIAL" install -r app/build/outputs/apk/debug/app-debug.apk
  adb -s "$SERIAL" shell am instrument -w ...
)
```

Never split `$out` before checking `rc`: on 75 it holds `ticket 7`, not a serial. The subshell makes the trap fire when the block ends, even if a step fails. `--json` prints `{"slot":id,"device":serial}` instead.

Facts: the ticket id is the slot id, so a resume prints that same id as `<slot> <serial>`. Hold time starts at the grant. If you lose the slot id, `devq status` shows it.

## Fallback: separate tool calls

Only if one call cannot work. With a 120s tool timeout use `--wait 60s` for a single attempt. On exit 75 (`ticket <id>`), your very next call must be `devq request --ticket <id> --wait 60s`. A ticket not polled for 8s is skipped when devices are handed out, and one not polled for 30s is abandoned. Do not think, edit code or sleep between attempts. Keep each `--wait` below your tool timeout. `--agent` and `--purpose` are not needed with `--ticket`. After the grant, always run `devq release <slot>`, including when a step fails.

## Be a good citizen

- Keep slots short. Release the moment the device work ends.
- Do not hold a slot while thinking or editing code. Build first, then acquire right before the device work.
- Write honest `--purpose` and `--description` text. Humans watch them on the dashboard.
- Do not change global device state the next agent depends on (animation scales, locale, installed apps, settings, orientation, an `adb root` switch), unless that is your purpose. Undo what you changed before releasing.
- Use `adb logcat -d` (dump and exit). A streaming logcat holds the slot until `--max` kills it.
- Do not use `--device` unless you need that exact device. Pinning makes you wait longer.

## Checking state

- `devq status` shows devices, holders and the queue. `--json` for scripts.
- `devq devices` lists the pool (serial, kind, state, model).
- `devq live --open` starts the dashboard for humans on 127.0.0.1:7878. Agents do not need it.

## Troubleshooting

| Exit | Meaning | Do this |
|---|---|---|
| 75 | `--wait` expired, still queued | Resume with `devq request --ticket <id>` at once, or use the loop block. |
| 1 | Slot ended (reason `timeout` or `orphaned`), unknown slot, or other error | Your `--max` passed, or the holder process died. The device is no longer yours. Request again, and raise `--max` if it timed out. |
| 2 | Bad arguments | Check required `--agent` and `--purpose`, duration syntax (`90s`, `10m`) and `--max` >= `--expect`. |
| 124 or 137 | `devq run` killed the command at `--max` | Raise `--max` if the work is legitimate. Otherwise find the hang. Never retry blindly in a loop. |
