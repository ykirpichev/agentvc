#!/usr/bin/env python3
"""Record tests/fixture.ts from a running agentvcd.

    python3 tests/record-fixture.py http://127.0.0.1:7420 /path/prefix/to/hide

Keys are path plus query, as the pane asks for them. The prefix (the demo
space's parent folder) is replaced by /home/me so no local path is committed.
"""

import json
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

base = sys.argv[1].rstrip("/")
hide = sys.argv[2] if len(sys.argv) > 2 else None
q = lambda s: urllib.parse.quote(s, safe="")


def get(path):
    try:
        with urllib.request.urlopen(base + path) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError:
        return None


out = {p: get(p) for p in ["/api/space", "/api/graph", "/api/tasks", "/api/live", "/api/workspaces"]}


def files(a_ref, b_ref, changes):
    for c in changes[:6]:
        for ref in (a_ref, b_ref):
            p = f"/api/file?ref={q(ref)}&path={q(c['path'])}"
            body = get(p)
            if body is not None:
                out[p] = body


for g in out["/api/graph"]["attempts"]:
    p = f"/api/diff?b=a{g['attempt']['id']}"
    d = get(p)
    if d is not None:
        out[p] = d
        files(d["a_ref"], d["b_ref"], d["changes"])

for w in out["/api/workspaces"]["workspaces"]:
    for b in w["branches"]:
        p = f"/api/workspaces/{q(w['id'])}/branches/" + "/".join(q(x) for x in b["name"].split("/"))
        d = get(p)
        if d is not None:
            out[p] = d
            files("git:" + d["base"], "git:" + d["tip"], d["files"])

text = json.dumps(out, indent=2, ensure_ascii=False)
if hide:
    text = text.replace(hide.rstrip("/"), "/home/me")
Path(__file__).with_name("fixture.ts").write_text(
    "// Answers recorded from agentvcd over a demo space with two windows (stacked\n"
    "// branches) and three versions, keyed by path and query. Re-record with\n"
    "// tests/record-fixture.py when the API shapes change.\n"
    "export const FIXTURE: Record<string, unknown> = " + text + "\n"
)
print(f"recorded {len(out)} answers")
