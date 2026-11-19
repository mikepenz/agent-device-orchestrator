# devq live: design notes

One design, built from `src/src.html` with `python3 src/build.py` (writes `live.html` and `src/artifact.html`). The artifact adds a tuning panel (simulator speed, device count, theme, density, force timeout, orphan or overrun). The A/B/C layout toggle from round one is gone: the user kept the timeline, so there is nothing left to compare.

## Layout
Flat sections divided by hairlines, no cards.
1. Header: name, glance line (`3/3 busy`, `2 waiting`), time window (1h, 6h, 24h), connection state.
2. Devices (left) and queue (320px, right).
3. Timeline, one lane per device, 34px bars. Click a bar to open the run.
4. Stats strip, nine figures.
5. Runs: filters (agent, device, outcome, search over purpose, description, command and agent/device), newest first. Click a row to open the detail drawer.
6. Per device and per agent tables.

Below 1000px everything stacks. At 375px the run log keeps agent, purpose and outcome; the rest is in the drawer.

## Tokens
- Neutrals only: white or `#0b0b0c`, ink `#0f0f10` or `#ededee`, three greys, two hairline tones. The only accent is ink (selected window, active badge). Agent colours are the 8 validated categorical slots, picked by FNV-1a of the agent name, so a name keeps its colour everywhere.
- State uses text colour: green Free, amber Overrun, red near limit or timeout. Nothing is tinted or glowing.
- Type: system sans, mono for serials, ids and commands, tabular numerals everywhere. Sizes 11.5 / 13 / 15 / 22. Sentence case, no uppercase labels.
- Radius 3 to 7px, one shadow-free surface. Bars 4px (cards) and 3px (timeline).

## No layout shift
Measured with a `layout-shift` observer on the simulator at 200x speed: round one 0.18 CLS in 40s, now 0.0006 in 57s.
- Device cells have fixed row heights, so free, busy and overrun are the same size. Free cells fade their detail rows instead of collapsing them.
- The queue and the run log live in fixed-height boxes with internal scroll. Rows are absolutely placed and moved with `transform`; enter and leave are opacity only.
- Countdown and elapsed labels sit in fixed-width slots, tables use `table-layout: fixed`, the legend scrolls on one line, tables sort by name so rows do not reorder.
- The detail drawer is an overlay.

## Motion
Linear 250ms width transitions on bars (fed by a 4 Hz clock), 300ms transform for queue and log rows, 450ms number tween, 200ms drawer slide, one 2.4s pulse on the live dot. All animation and transition durations drop to zero under `prefers-reduced-motion`.

## Data
Polls `/api/state?window=<secs>` every 1s with a 5s timeout. Responses that fail shape checks are treated as failures. The simulator runs for `?mock=1` or until the API has answered once; after that a failure shows "offline, retrying" and keeps the last real state. A window switch fetches at once and drops stale replies.
`history` items carry description, command, want_device, max_secs, and may have `granted_at` null (abandoned, cancelled; no device). All agent text goes in with `textContent`.
