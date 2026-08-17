# fixtures/ — recorded ground truth for orchestrator-host tests

RULE ZERO (docs/013 §3): `cargo test` NEVER spawns claude/codex. Tests replay these recordings.
Captured live on this machine 2026-06-10 (S1/S2 verification, docs/012 §7) against the pinned
CLIs. Re-record with a capture harness run ONLY against pinned versions; regenerating burns plan
quota — treat these files as irreplaceable.

claude/2.1.172/
  pty/          raw PTY byte streams from hosted interactive sessions:
                claude_raw.log   first hosted run (folder-trust gate + Bash permission dialog)
                claude_raw2.log  Bash dialog + digit answer + OSC title/progress traffic
                claude_raw3.log  Write permission dialog ("Do you want to create …?") — the
                                 grid-not-stream regression fixture (013 §3 test #4)
                claude_raw4.log  bracketed-paste / multi-line input run
  hooks/        verbatim hook stdin payloads: permreq.log (PermissionRequest), pretool.log
                (PreToolUse incl. tool_use_id), stop.log, notif.log
  headless/     claude -p runs with hook-returned decisions (allow/deny outputs)
  transcripts/  session jsonls incl. the Esc-rejection tool_result{is_error,tool_use_id}
                records (the hook-invisible rejection join, 012 §1.5)
  settings/     the capture hook settings template + capture.sh
codex/0.132.0/
  pty/          codex TUI byte stream incl. the exec-approval modal render
  notify/       notify program payload (agent-turn-complete JSON)
reports/        full S1/S2 live-verification + control-surfaces research outputs (verbatim
                evidence archive; the wire frames for app-server dual-attach live here)
