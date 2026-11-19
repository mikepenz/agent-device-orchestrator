#!/usr/bin/env python3
"""Assemble the docs site: pages/*.html fragments + layout.html -> _site/. Stdlib only."""
import re, shutil
from pathlib import Path

HERE = Path(__file__).parent
OUT = HERE / "_site"

# (file, sidebar title, page title, description)
GROUPS = [
    ("Overview", [("index", "Get Started"), ("how-it-works", "How it works")]),
    ("Usage", [("commands", "Commands")]),
    ("Agents", [("agents", "Agent setup")]),
    ("Dashboard", [("dashboard", "Live dashboard")]),
]
DESC = {
    "index": "devq: a fair queue for shared Android devices between AI agents. One binary, no daemon, a live dashboard.",
    "how-it-works": "How devq coordinates agents through one SQLite file, and where it runs.",
    "commands": "devq commands, slot flags, exit codes and config.",
    "agents": "Teach agents to use devq: instructions, the bundled skill and the enforcement hook.",
    "dashboard": "devq live: the local dashboard, running it as a Mac app, and querying the database.",
}
CUR = ' aria-current="page"'
pages = [p for _, g in GROUPS for p in g]
order = [p[0] for p in pages]


def sidebar(cur):
    out = []
    for name, items in GROUPS:
        lis = "".join(
            f'<li><a href="{"./" if f == "index" else f + ".html"}"{CUR if f == cur else ""}>{t}</a></li>'
            for f, t in items
        )
        out.append(f"        <h3>{name}</h3>\n        <ul>{lis}</ul>")
    return "\n".join(out)


def build():
    shutil.rmtree(OUT, ignore_errors=True)
    OUT.mkdir()
    for f in ("site.css", "icon.svg", "apple-touch-icon.png"):
        shutil.copy(HERE / f, OUT / f)
    shutil.copytree(HERE / "fonts", OUT / "fonts")
    shutil.copytree(HERE.parent / "art", OUT / "art")
    layout = (HERE / "layout.html").read_text()
    titles = dict(pages)
    for i, f in enumerate(order):
        body = (HERE / "pages" / f"{f}.html").read_text()
        h2s = re.findall(r'<h2 id="([^"]+)">(.*?)</h2>', body)
        toc = "        <h3>On this page</h3>\n        <ul>" + "".join(f'<li><a href="#{a}">{t}</a></li>' for a, t in h2s) + "</ul>"
        toc += '\n        <div class="more"><a href="https://github.com/mikepenz/agent-device-orchestrator#reference">Full reference on GitHub ↗</a></div>'
        prev = f'<a href="{"./" if order[i-1] == "index" else order[i-1] + ".html"}">← {titles[order[i-1]]}</a>' if i else "<span></span>"
        nxt = f'<a href="{order[i+1]}.html">{titles[order[i+1]]} →</a>' if i + 1 < len(order) else "<span></span>"
        title = "device-pool-orchestrator" if f == "index" else f"{titles[f]} · devq"
        html = layout
        for k, v in {
            "title": title, "desc": DESC[f], "sidebar": sidebar(f), "body": body, "toc": toc,
            "pager": f'          <div class="pager">{prev}{nxt}</div>',
            "nav_docs": ' aria-current="page"' if f in ("index", "how-it-works") else "",
            "nav_commands": ' aria-current="page"' if f == "commands" else "",
            "nav_agents": ' aria-current="page"' if f == "agents" else "",
        }.items():
            html = html.replace("{{" + k + "}}", v)
        (OUT / ("index.html" if f == "index" else f"{f}.html")).write_text(html)


if __name__ == "__main__":
    build()
    print(f"built {len(order)} pages -> {OUT}")
