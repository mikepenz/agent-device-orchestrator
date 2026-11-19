#!/usr/bin/env python3
"""Generate art/hero-light.svg and art/hero-dark.svg from one description.
Palette = the dashboard tokens in design/src/src.html. Usage: python3 hero.py"""
import os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "art")
SANS = "ui-sans-serif,system-ui,-apple-system,'Segoe UI',Roboto,'Helvetica Neue',Arial,sans-serif"
MONO = "ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace"

THEMES = {
    "light": dict(bg="#ffffff", fg="#0f0f10", fg2="#55565b", fg3="#85868c", line="#d9d9de", hair="#ececee",
                  blue="#2a78d6", good="#17803d", agents=["#2a78d6", "#eb6834", "#1baf7a", "#6250d6"], onblue="#ffffff"),
    "dark": dict(bg="#0b0b0c", fg="#ededee", fg2="#a1a1a7", fg3="#85868c", line="#2d2d32", hair="#1d1d20",
                 blue="#3987e5", good="#3fb96a", agents=["#3987e5", "#d95926", "#199e70", "#9085e9"], onblue="#ffffff"),
}
W, H = 1200, 420
AGENTS = ["claude", "codex", "gemini", "subagent"]
DEVICES = [("emulator-5554", "busy"), ("phone", "busy"), ("rig-a", "static")]
AY = [172, 217, 262, 307]
DY = [193, 238, 283]


def svg(t):
    o = []
    a = o.append
    def text(x, y, s, size, fill, mono=False, weight=400, anchor="start"):
        fam = MONO if mono else SANS
        a(f'<text x="{x}" y="{y}" font-family="{fam}" font-size="{size}" font-weight="{weight}" '
          f'fill="{fill}" text-anchor="{anchor}">{s}</text>')
    def hl(x1, y1, x2, y2):
        a(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{t["line"]}" stroke-width="1.5"/>')

    a(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-labelledby="t d">')
    a('<title id="t">devq</title>')
    a('<desc id="d">System map: agents (Claude, Codex, Gemini, subagents) request a slot from a FIFO queue stored in one SQLite file '
      'with no daemon. The queue hands out exclusive slots to devices (an emulator, a phone, a static resource). '
      'devq live watches all of it.</desc>')
    a(f'<rect width="{W}" height="{H}" fill="{t["bg"]}"/>')
    a(f'<rect x=".75" y=".75" width="{W-1.5}" height="{H-1.5}" rx="12" fill="none" stroke="{t["line"]}" stroke-width="1.5"/>')
    text(56, 78, "devq", 56, t["fg"], weight=700)
    text(204, 76, "A fair queue for shared devices. One SQLite file, no daemon.", 22, t["fg2"])

    cols = [(56, "agents ask"), (350, "queue"), (700, "slots"), (840, "devices")]
    for x, s in cols:
        text(x, 128, s, 18, t["fg3"])

    # agents
    for y, name, c in zip(AY, AGENTS, t["agents"]):
        a(f'<rect x="56" y="{y-7}" width="14" height="14" rx="3" fill="{c}"/>')
        text(82, y + 7, name, 20, t["fg"], mono=True)
        hl(190, y, 350, y)

    # queue box: the three bars of the icon, one per ticket
    a(f'<rect x="350" y="146" width="290" height="180" rx="10" fill="none" stroke="{t["line"]}" stroke-width="1.5"/>')
    text(372, 180, "devq.db", 22, t["fg"], mono=True, weight=600)
    text(372, 206, "FIFO, tickets, exclusive", 18, t["fg2"])
    for i, (c, tid, op) in enumerate(zip([t["agents"][1], t["agents"][2], t["agents"][3]], ["#41", "#42", "#43"], [.55, 1, .55])):
        y = 226 + i * 30
        a(f'<rect x="372" y="{y}" width="130" height="20" rx="5" fill="{c}" fill-opacity="{op}"/>')
        text(518, y + 16, tid, 18, t["fg2"], mono=True)

    # queue -> slots -> devices
    for y in DY:
        hl(640, y, 700, y)
        a(f'<rect x="700" y="{y-18}" width="36" height="36" rx="8" fill="{t["blue"]}"/>')
        text(718, y + 7, str(DY.index(y) + 1), 20, t["onblue"], mono=True, weight=700, anchor="middle")
        hl(736, y, 840, y)
    for y, (name, state) in zip(DY, DEVICES):
        a(f'<rect x="840" y="{y-19}" width="304" height="38" rx="8" fill="none" stroke="{t["line"]}" stroke-width="1.5"/>')
        text(858, y + 7, name, 20, t["fg"], mono=True)
        text(1126, y + 6, state, 18, t["good"] if state == "busy" else t["fg2"], anchor="end")

    # live bar
    a(f'<rect x="56" y="350" width="1088" height="48" rx="10" fill="none" stroke="{t["line"]}" stroke-width="1.5"/>')
    a(f'<circle cx="82" cy="374" r="6" fill="{t["blue"]}"/>')
    text(100, 381, "devq live", 20, t["fg"], mono=True, weight=600)
    text(224, 381, "watches the same file: devices, queue, timeline, run log", 20, t["fg2"])
    a('</svg>')
    return "\n".join(o) + "\n"


os.makedirs(OUT, exist_ok=True)
for name, t in THEMES.items():
    with open(os.path.join(OUT, f"hero-{name}.svg"), "w") as f:
        f.write(svg(t))
