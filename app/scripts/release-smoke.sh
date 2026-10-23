#!/usr/bin/env bash
#
# Smoke-test the RELEASE ARTIFACT, launched the way a user launches it, BEFORE
# it is published. Run it after make-dmg.sh and before tagging or uploading.
#
#   scripts/release-smoke.sh                  # ../dist/Kod.dmg
#   scripts/release-smoke.sh path/to/Kod.dmg  # or a Kod.app
#   scripts/release-smoke.sh --stop           # stop a leftover run, delete the sandbox
#
# ---------------------------------------------------------------------------
# WHY THIS EXISTS
#
# Kod 0.4.6 shipped with "Restore all" dead (found 2026-10-23). After a reboot
# macOS reopened Kod as a login item with launchd's environment —
# PATH=/usr/bin:/bin:/usr/sbin:/sbin — on which neither claude nor codex exists.
# Every restore failed, and the error was drawn only on a screen the button
# never leads to. Every test before that release ran Kod from a terminal, which
# hands it the terminal's PATH, so none of them could have seen it.
#
# So this tests the artifact that ships, the way it ships: the notarized DMG,
# copied out the way a user would, opened through LaunchServices (`open`), which
# gives it launchd's environment, then made to restore a claude and a codex
# session that "crashed". It passes only if both come back AND are still running
# a few seconds later — a resume that dies at once is not a restore.
#
# ISOLATION. Two roots are redirected; one deliberately is NOT:
#   KOD_STORE_DIR    → a throwaway store holding only the two test sessions
#   XDG_RUNTIME_DIR  → a throwaway daemon socket, so your real daemon is untouched
#   HOME             → stays REAL. What is under test is Kod loading YOUR login
#                      shell's environment; a sandboxed HOME would hide your rc
#                      files. Under $HOME the app under test only reads (transcript
#                      scans); the two test sessions in ~/.claude and ~/.codex are
#                      deleted when the run ends.
# `open --env` is how the redirects reach the app, so step 0 PROVES it delivers
# them, with a throwaway probe app, before Kod is launched at all. The app also
# refuses ORCH_DEMO=restore-all without KOD_STORE_DIR.
#
# COST: two one-word model calls (one `claude -p`, one `codex exec`), to make
# sessions real enough to resume.
# ---------------------------------------------------------------------------

set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# Not under /tmp (registry.rs drops any project there) and SHORT: the daemon
# socket path inside it must stay under the 104-byte unix-socket limit.
SANDBOX=/Users/Shared/kod-smoke
RUNTIME="$SANDBOX/run"
STORE="$SANDBOX/store"
PROJ="$SANDBOX/proj"
UNDER_TEST="$SANDBOX/app/Kod.app"
LOG="$SANDBOX/kod.log"

sock_for() { python3 -c '
import os, sys
d = sys.argv[1] or os.environ.get("TMPDIR", "/tmp")
print(os.path.join(d, "orchestrator", "daemon.sock"))
' "$1"; }
REAL_SOCK="$(sock_for "${XDG_RUNTIME_DIR:-}")"
SBOX_SOCK="$(sock_for "$RUNTIME")"
if [ "$REAL_SOCK" = "$SBOX_SOCK" ]; then
  echo "ERROR: sandbox socket == real socket ($REAL_SOCK) — refusing." >&2
  exit 1
fi

# Stop ONLY what this sandbox started: the holder of the sandbox socket and
# processes running from the sandbox's own copy of the bundle. Never a name
# match — your real Kod runs binaries with the same names.
stop_sandbox() {
  if [ -S "$SBOX_SOCK" ]; then
    for p in $(lsof -t -- "$SBOX_SOCK" 2>/dev/null || true); do kill "$p" 2>/dev/null || true; done
  fi
  for p in $(pgrep -f "^$UNDER_TEST/Contents/MacOS/" 2>/dev/null || true); do
    kill "$p" 2>/dev/null || true
  done
}

# Delete the test sessions' transcripts — and only those: the claude folder is
# removed only when it is the one named after $PROJ.
forget_test_sessions() {
  if [ -f "$SANDBOX/claude.id" ]; then
    local t
    t="$(find "$HOME/.claude/projects" -maxdepth 2 -name "$(cat "$SANDBOX/claude.id").jsonl" 2>/dev/null | head -1)"
    case "$(dirname "${t:-/}")" in
      */-Users-Shared-kod-smoke-proj) rm -rf "$(dirname "$t")";;
    esac
  fi
  if [ -f "$SANDBOX/codex.id" ] && [ -s "$SANDBOX/codex.id" ]; then
    find "$HOME/.codex/sessions" -name "rollout-*-$(cat "$SANDBOX/codex.id").jsonl" -delete 2>/dev/null || true
  fi
}

if [ "${1:-}" = "--stop" ]; then
  stop_sandbox
  forget_test_sessions
  rm -rf "$SANDBOX"
  echo "removed $SANDBOX"
  exit 0
fi

TARGET="${1:-$APP_DIR/../dist/Kod.dmg}"
[ -e "$TARGET" ] || { echo "ERROR: $TARGET not found" >&2; exit 1; }

stop_sandbox
forget_test_sessions
rm -rf "$SANDBOX"
mkdir -p "$RUNTIME" "$STORE" "$PROJ" "$SANDBOX/app"
chmod 700 "$RUNTIME" "$STORE"
trap 'stop_sandbox; forget_test_sessions' EXIT

FAILED=0
pass() { echo "  PASS  $*"; }
fail() { echo "  FAIL  $*"; FAILED=1; }

# `open` hands the app ITS OWN environment (measured: run from a terminal, the
# app got the terminal's whole PATH — and a test run that way passed without
# the fix). So `open` itself runs in what launchd gives a login item or a Dock
# click, and step 0 checks that this is what arrives.
LAUNCHD_PATH=/usr/bin:/bin:/usr/sbin:/sbin
launchd_open() {
  env -i HOME="$HOME" USER="$USER" LOGNAME="$USER" SHELL="${SHELL:-/bin/zsh}" \
    PATH="$LAUNCHD_PATH" TMPDIR="$TMPDIR" /usr/bin/open "$@"
}

# --- 0. prove `open --env` reaches the app -----------------------------------
PROBE="$SANDBOX/EnvProbe.app"
mkdir -p "$PROBE/Contents/MacOS"
cat > "$PROBE/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>probe</string>
  <key>CFBundleIdentifier</key><string>ai.felis.kod.smoke-probe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
PLIST
# A real Mach-O: LaunchServices refuses a script as a bundle executable
# (-10669). It writes its environment where step 0 can read it, and exits.
printf '%s\n' '#include <stdio.h>' 'extern char **environ;' \
  'int main(void) { FILE *f = fopen(OUT, "w"); if (!f) return 1;' \
  '  for (char **e = environ; *e; e++) fprintf(f, "%s\n", *e); return fclose(f); }' \
  > "$SANDBOX/probe.c"
cc -DOUT="\"$SANDBOX/probe.env\"" -o "$PROBE/Contents/MacOS/probe" "$SANDBOX/probe.c"
echo "==> 0. checking that open --env delivers the sandbox to an app"
launchd_open -n -g --env KOD_STORE_DIR="$STORE" --env XDG_RUNTIME_DIR="$RUNTIME" "$PROBE"
for _ in $(seq 50); do [ -s "$SANDBOX/probe.env" ] && break; sleep 0.1; done
if ! grep -qx "KOD_STORE_DIR=$STORE" "$SANDBOX/probe.env" 2>/dev/null \
  || ! grep -qx "XDG_RUNTIME_DIR=$RUNTIME" "$SANDBOX/probe.env" 2>/dev/null; then
  echo "ERROR: open --env did not deliver the sandbox — refusing to launch Kod," >&2
  echo "       it would open your REAL store and daemon." >&2
  exit 1
fi
pass "open --env delivers KOD_STORE_DIR + XDG_RUNTIME_DIR"
if ! grep -qx "PATH=$LAUNCHD_PATH" "$SANDBOX/probe.env"; then
  echo "ERROR: the app would start with $(grep '^PATH=' "$SANDBOX/probe.env"), not launchd's" >&2
  echo "       PATH — this run would not reproduce a Dock launch. Refusing." >&2
  exit 1
fi
pass "the app starts with launchd's PATH, as from the Dock or a login item"

# --- 1. the artifact ---------------------------------------------------------
echo "==> 1. artifact: $TARGET"
case "$TARGET" in
  *.dmg)
    xcrun stapler validate "$TARGET" >/dev/null 2>&1 \
      && pass "DMG notarization ticket is stapled" || fail "DMG has no stapled ticket"
    MNT="$SANDBOX/mnt"
    mkdir -p "$MNT"
    hdiutil attach -nobrowse -readonly -quiet -mountpoint "$MNT" "$TARGET"
    /usr/bin/ditto "$MNT/Kod.app" "$UNDER_TEST"
    hdiutil detach -quiet "$MNT"
    ;;
  *.app) /usr/bin/ditto "$TARGET" "$UNDER_TEST";;
  *) echo "ERROR: expected a .dmg or a .app" >&2; exit 1;;
esac
VERSION="$(defaults read "$UNDER_TEST/Contents/Info.plist" CFBundleShortVersionString)"
WANT="$(sed -n 's/^version = "\(.*\)"/\1/p' "$APP_DIR/crates/orchestrator-gui/Cargo.toml" | head -1)"
[ "$VERSION" = "$WANT" ] && pass "version $VERSION" || fail "version $VERSION, expected $WANT"
codesign --verify --deep --strict "$UNDER_TEST" 2>/dev/null \
  && pass "signature verifies (deep, strict)" || fail "signature does not verify"
spctl -a -t exec "$UNDER_TEST" 2>/dev/null \
  && pass "Gatekeeper accepts it" || fail "Gatekeeper rejects it: $(spctl -a -t exec -vv "$UNDER_TEST" 2>&1 | tail -1)"

# --- 2. two real sessions, then a fake crash ----------------------------------
echo "==> 2. making a claude and a codex session in $PROJ (two one-word model calls)"
CLAUDE_ID="$(uuidgen | tr 'A-Z' 'a-z')"
echo "$CLAUDE_ID" > "$SANDBOX/claude.id"
(cd "$PROJ" && claude -p --session-id "$CLAUDE_ID" "Reply with the single word: ok" </dev/null >/dev/null)
CODEX_ID="$(cd "$PROJ" && codex exec --skip-git-repo-check --json "Reply with the single word: ok" </dev/null 2>/dev/null \
  | jq -r 'select(.type == "thread.started") | .thread_id' | head -1)"
echo "$CODEX_ID" > "$SANDBOX/codex.id"
[ -n "$CODEX_ID" ] || { echo "ERROR: codex exec reported no thread id" >&2; exit 1; }
(cd "$APP_DIR" && KOD_STORE_DIR="$STORE" cargo run -q -p orchestrator-store --example seed_restore -- \
  "claude:$CLAUDE_ID:$PROJ" "codex:$CODEX_ID:$PROJ")

# --- 3. launch it like the Dock does, and press Restore all --------------------
echo "==> 3. opening the app through LaunchServices with ORCH_DEMO=restore-all"
launchd_open -n -g --stderr "$LOG" \
  --env KOD_STORE_DIR="$STORE" --env XDG_RUNTIME_DIR="$RUNTIME" --env ORCH_DEMO=restore-all \
  "$UNDER_TEST"
for _ in $(seq 300); do grep -q '^\[kod\] restore-all:' "$LOG" 2>/dev/null && break; sleep 0.1; done
grep '^\[kod\] ' "$LOG" | sed 's/^/        /' || true
grep -q '^\[kod\] environment: Shell' "$LOG" \
  && pass "loaded the login shell's environment" || fail "did not load the login shell's environment"
grep -q '^\[kod\] restore-all: offered 2, left unrestored 0, error: None$' "$LOG" \
  && pass "Restore all restored both sessions" || fail "Restore all did not restore both sessions"

# --- 4. are they actually running? -------------------------------------------
sleep 8
DAEMON="$(pgrep -f "^$UNDER_TEST/Contents/MacOS/orchestrator-daemon" | head -1 || true)"
if [ -z "$DAEMON" ]; then
  fail "no daemon running from the app under test"
else
  KIDS="$(ps -axo ppid=,command= | awk -v d="$DAEMON" '$1 == d { $1 = ""; print }')"
  echo "$KIDS" | grep -q "claude.*--resume" \
    && pass "claude --resume is still running 8s later" || fail "no claude running under the daemon"
  echo "$KIDS" | grep -q "codex.*resume $CODEX_ID" \
    && pass "codex resume is still running 8s later" || fail "no codex running under the daemon"
  ps -E -ww -o command= -p "$DAEMON" | tr ' ' '\n' | grep -q "^PATH=.*$HOME/.local/bin" \
    && pass "the daemon's PATH has ~/.local/bin" || fail "the daemon's PATH is still launchd's"
fi

# --- 5. what it looked like ----------------------------------------------------
GUI="$(pgrep -f "^$UNDER_TEST/Contents/MacOS/kod( |\$)" | head -1 || true)"
SHOT="$APP_DIR/../docs/map-tour-2026-10-05/winshot.py"
if [ -n "$GUI" ] && [ -f "$SHOT" ]; then
  python3 "$SHOT" "$GUI" "$SANDBOX/after-restore" >/dev/null 2>&1 \
    && echo "        screenshot: $(ls "$SANDBOX"/after-restore* 2>/dev/null | head -1)"
fi

echo
if [ "$FAILED" = 0 ]; then
  echo "SMOKE PASSED — Kod $VERSION, opened like the Dock opens it, restored a crashed claude and codex."
else
  echo "SMOKE FAILED — do NOT publish. Log: $LOG"
  exit 1
fi
