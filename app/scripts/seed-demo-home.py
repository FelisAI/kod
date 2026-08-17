#!/usr/bin/env python3
"""Build a throwaway HOME with a believable project rail, for README screenshots.

    python3 scripts/seed-demo-home.py                 # writes /Users/Shared/koddemo
    HOME=/Users/Shared/koddemo ORCH_DEMO=sessions ORCH_NO_DAEMON=1 \
        ./target/debug/orchestrator

Then capture the window (see capture note at the bottom of this file). Nothing
here touches your real store: the app derives its data directory from $HOME, so
pointing HOME at the demo tree redirects everything.

TWO NON-OBVIOUS CONSTRAINTS, both found the hard way — do not "simplify" past them:

1. THE DEMO HOME MUST NOT LIVE UNDER /tmp. registry.rs hard-filters any candidate
   cwd under /tmp, /private/tmp, or /private/var/folders — a temp path is never a
   project. Seeding into a tempdir yields sources but ZERO rail rows, silently.
   /Users/Shared is the natural non-temp scratch location on macOS.

2. THE TRANSCRIPT JSON MUST BE COMPACT. The scanner looks for the literal
   `"cwd":"` with no space, so json.dumps' default `": "` separator makes the cwd
   unparseable — sources are found, dominant_cwd is None, and again you get zero
   rows. Hence separators=(",", ":").

Directory names under .claude/projects are the cwd with "/" replaced by "-", which
is what real Claude Code writes. Avoid demo paths containing "-": the encoding is
ambiguous and cannot be round-tripped back to a directory.
"""

import json
import os
import shutil
import sys
import time
import uuid

DEFAULT_HOME = "/Users/Shared/koddemo"

# (project, [(seconds_ago, what the session last said)]). The mtime is what the
# rail sorts and labels by, so staggering these is what makes the demo read as a
# real week of work rather than four rows all saying "now".
DEMO = {
    "atlas": [
        (4 * 60, "Split the ingest pipeline into three stages"),
        (52 * 60, "Fixed the duplicate-key crash on replay"),
        (3 * 3600, "p99 write latency down from 240ms to 31ms"),
    ],
    "harbor": [
        (18 * 60, "Added a readiness gate before cutover"),
        (26 * 3600, "Rolled the canary to 25% — error rate clean"),
    ],
    "ledger": [
        (2 * 3600, "Reconciliation balances across the backfill"),
    ],
    "beacon": [
        (40 * 60, "Switched the notifier to exponential backoff"),
        (5 * 3600, "Added a dead-letter queue for bad payloads"),
    ],
}


def main():
    home = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_HOME
    if home.startswith("/tmp") or home.startswith("/private/tmp"):
        sys.exit(f"refusing {home}: see constraint 1 in this file's docstring")

    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(f"{home}/Library/Application Support/orchestrator", exist_ok=True)
    now = time.time()

    for name, sessions in DEMO.items():
        cwd = f"{home}/local/{name}"
        os.makedirs(f"{cwd}/src", exist_ok=True)
        with open(f"{cwd}/README.md", "w") as f:
            f.write(f"# {name}\n")
        d = f"{home}/.claude/projects/{cwd.replace('/', '-')}"
        os.makedirs(d, exist_ok=True)
        for ago, said in sessions:
            sid = str(uuid.uuid4())
            p = f"{d}/{sid}.jsonl"
            with open(p, "w") as f:
                for line in (
                    {"type": "mode", "mode": "normal", "sessionId": sid},
                    {"type": "user", "cwd": cwd, "sessionId": sid,
                     "message": {"role": "user", "content": "keep going"}},
                    {"type": "assistant", "cwd": cwd, "sessionId": sid,
                     "message": {"role": "assistant",
                                 "content": [{"type": "text", "text": said}]}},
                ):
                    f.write(json.dumps(line, separators=(",", ":")) + "\n")
            os.utime(p, (now - ago, now - ago))

    n = sum(len(v) for v in DEMO.values())
    print(f"seeded {home}: {len(DEMO)} projects, {n} sessions")
    print("run:  HOME=%s ORCH_DEMO=sessions ORCH_NO_DAEMON=1 ./target/debug/orchestrator" % home)


# Capturing: screenshot the window by ID rather than by screen region, so a
# second running copy of the app can never end up in the frame:
#
#   screencapture -l"$(python3 - "$PID" <<'P'
#   import subprocess,sys
#   P
#   )" -o -x shot.png
#
# The simplest reliable way to get the id is Quartz's CGWindowListCopyWindowInfo
# filtered by the demo process's pid; on a machine with Xcode, a four-line Swift
# script does it. Region capture (-R) is the trap: it will happily photograph
# whichever window happens to be frontmost, including your real one.
if __name__ == "__main__":
    main()
