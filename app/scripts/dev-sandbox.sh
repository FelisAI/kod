#!/usr/bin/env bash
#
# Run Kod against a THROWAWAY world, so you can test a build while your real
# sessions keep running untouched.
#
#   scripts/dev-sandbox.sh              # isolated daemon + demo projects
#   scripts/dev-sandbox.sh --empty      # isolated daemon, no seeded data
#   scripts/dev-sandbox.sh --no-daemon  # in-process host, no daemon at all
#   scripts/dev-sandbox.sh --stop       # stop the sandbox daemon, delete the sandbox
#
# ---------------------------------------------------------------------------
# WHY THIS EXISTS, AND WHY $HOME ALONE IS NOT ENOUGH
#
# Kod reads its world from TWO independent roots, and redirecting only the first
# one is the trap:
#
#   $HOME              → the store (Library/Application Support/orchestrator),
#                        ~/.claude/projects, ~/.codex/sessions
#   $XDG_RUNTIME_DIR   → the DAEMON SOCKET (falling back to $TMPDIR when unset;
#     (or $TMPDIR)       see daemon::default_socket_path)
#
# The socket does NOT follow $HOME. So a "sandboxed" GUI launched with only HOME
# redirected still connects to the socket your REAL daemon is listening on — and
# then this happens (daemon::attach_gate):
#
#   WireGate::Accept if rebuilt  =>  AttachGate::Retire
#
# `rebuilt` means the on-disk binary is newer than the one the running daemon
# launched from — which is true every time you `cargo build`. A rebuilt binary
# retires the live daemon exactly like an outdated wire would. That is GOOD
# ergonomics in normal dev (no `pkill` needed between builds) and catastrophic
# when the daemon you retire owns a dozen live agent sessions.
#
# So this script redirects BOTH roots, and then asserts it actually worked
# before launching anything.
# ---------------------------------------------------------------------------

set -euo pipefail

APP="$(cd "$(dirname "$0")/.." && pwd)"
cd "$APP"

SANDBOX="${KOD_SANDBOX:-/Users/Shared/koddemo}"
case "$SANDBOX" in
  /tmp/*|/private/tmp/*)
    echo "ERROR: the sandbox must not live under /tmp — registry.rs hard-filters" >&2
    echo "       any project path under /tmp, so the rail would come up empty." >&2
    exit 1;;
esac

MODE=seeded
SNAPSHOT=0
BUNDLE=0
NOTIFY=0
for a in "$@"; do
  case "$a" in
    --empty)     MODE=empty;;
    # Boot against a SNAPSHOT of your real store, so Standup renders your actual
    # history instead of invented rows. Implies --empty: seeded demo projects
    # mixed with real ones is a confusing hybrid that is neither.
    #
    # SAFETY: this READS your store and writes only into the sandbox. It uses
    # sqlite3 .backup rather than cp — a plain copy misses whatever is still in
    # the -wal file, which is exactly the most recent activity you want to see.
    --snapshot)  SNAPSHOT=1; MODE=empty;;
    --no-daemon) MODE=nodaemon;;
    --stop)      MODE=stop;;
    # Run from Kod.app instead of the bare binary. This is what gives the real
    # app IDENTITY: the menu-bar title reads "Kod" (macOS takes it from the
    # bundle, NOT from gpui's set_menus) and the Dock shows kod.icns. The bare
    # binary can do neither — it has no Info.plist to read them from.
    --bundle)    BUNDLE=1;;
    # Fire one test notification 3s after boot. Implies --bundle, because the
    # whole point is seeing which app macOS attributes the banner to.
    --notify)    BUNDLE=1; NOTIFY=1;;
    -h|--help)   sed -n '2,12p' "$0"; exit 0;;
    *) echo "unknown flag: $a" >&2; exit 1;;
  esac
done

RUNTIME="$SANDBOX/run"

# --- the guard that makes this trustworthy ----------------------------------
# Compute the socket path the REAL app uses (no overrides) and the one the
# sandbox will use, and refuse to continue unless they differ. This is the one
# invariant that protects live sessions, so it is asserted rather than assumed.
real_sock() { python3 -c '
import os
d = os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR", "/tmp")
print(os.path.join(d, "orchestrator", "daemon.sock"))
'; }
REAL_SOCK="$(env -u XDG_RUNTIME_DIR bash -c "$(declare -f real_sock); real_sock")"
SBOX_SOCK="$(XDG_RUNTIME_DIR="$RUNTIME" bash -c "$(declare -f real_sock); real_sock")"

if [ "$REAL_SOCK" = "$SBOX_SOCK" ]; then
  echo "ERROR: sandbox socket == real socket ($REAL_SOCK)." >&2
  echo "       Refusing to launch: attaching would RETIRE your live daemon." >&2
  exit 1
fi

if [ "$MODE" = stop ]; then
  # NEVER `pkill -f orchestrator-daemon` here. Your real daemon runs from the same
  # binary path, so a name match kills it — and every live session with it. The
  # only safe identifier is "the process holding the SANDBOX socket", so ask lsof
  # which pid owns that exact file and kill nothing else.
  if [ -S "$SBOX_SOCK" ]; then
    pids="$(lsof -t -- "$SBOX_SOCK" 2>/dev/null || true)"
    for p in $pids; do
      kill "$p" 2>/dev/null && echo "stopped sandbox daemon (pid $p)"
    done
  fi
  rm -rf "$SANDBOX"
  echo "removed $SANDBOX"
  exit 0
fi

# BOTH modes start from nothing. --empty used to only SKIP the seeder, and the
# rm lived inside the seeder — so running --empty over an already-seeded sandbox
# silently kept the demo projects and handed you a stale world labelled "empty".
# That defeats the only thing --empty is for: the genuine first-run path, where
# the portfolio really is empty and the sentinel/scan guards decide what happens.
rm -rf "$SANDBOX"
mkdir -p "$RUNTIME" "$SANDBOX/Library/Application Support/orchestrator"
chmod 700 "$RUNTIME"

if [ "$MODE" = seeded ]; then
  python3 "$APP/scripts/seed-demo-home.py" "$SANDBOX" >/dev/null
fi

if [ "$SNAPSHOT" = 1 ]; then
  REAL_STORE="$HOME/Library/Application Support/orchestrator/store.db"
  SBOX_STORE="$SANDBOX/Library/Application Support/orchestrator/store.db"
  if [ ! -f "$REAL_STORE" ]; then
    echo "ERROR: no store at $REAL_STORE — nothing to snapshot." >&2
    exit 1
  fi
  command -v sqlite3 >/dev/null || { echo "ERROR: sqlite3 not found." >&2; exit 1; }
  # .backup, not cp: it checkpoints the WAL into the copy, so the newest events
  # (the ones you actually want to look at) are present. It only reads the source.
  sqlite3 "$REAL_STORE" ".backup '$SBOX_STORE'" || {
    echo "ERROR: snapshot failed" >&2; exit 1; }
  echo "==> snapshotted your real store -> sandbox ($(du -h "$SBOX_STORE" | cut -f1))"
  # Prove the copy is readable and carries history, before the app opens it.
  sqlite3 "$SBOX_STORE" \
    "SELECT '    ' || COUNT(*) || ' summaries across ' || COUNT(DISTINCT project_key)
     || ' projects' FROM session_summary;" 2>/dev/null || true
fi

cargo build -p orchestrator-gui -p orchestrator-daemon

BIN=./target/debug/orchestrator
IDENTITY="bare binary — menu bar will read \"orchestrator\", no Dock icon"
if [ "$BUNDLE" = 1 ]; then
  bash "$APP/scripts/make-app.sh" >/dev/null
  # Exec the binary INSIDE the bundle rather than `open`ing the app: `open`
  # launches via LaunchServices and would drop HOME/XDG_RUNTIME_DIR, silently
  # pointing the "sandbox" at the real store and the real daemon socket.
  BIN="$APP/../Kod.app/Contents/MacOS/kod"
  IDENTITY="Kod.app — menu bar reads \"Kod\", Dock shows kod.icns"
fi

echo "── sandbox ──────────────────────────────────────────────"
echo "  HOME             $SANDBOX"
echo "  store            $SANDBOX/Library/Application Support/orchestrator/store.db"
echo "  daemon socket    $SBOX_SOCK"
echo "  your real socket $REAL_SOCK   (untouched)"
echo "  identity         $IDENTITY"
[ "$MODE" = nodaemon ] && echo "  host             in-process (ORCH_NO_DAEMON=1)"
[ "$MODE" = seeded ]   && echo "  seeded           4 demo projects"
if [ "$NOTIFY" = 1 ]; then
  echo "  notify test      one banner ~3s after launch"
  echo "                   UNSIGNED bundle => the native path is taken but macOS"
  echo "                   may never grant authorization, so NOTHING appears."
  echo "                   Silence here is a real result, not a hang (see #51)."
fi
echo "─────────────────────────────────────────────────────────"

env_args=(HOME="$SANDBOX" XDG_RUNTIME_DIR="$RUNTIME")
[ "$MODE" = nodaemon ] && env_args+=(ORCH_NO_DAEMON=1)
[ "$NOTIFY" = 1 ]      && env_args+=(ORCH_NOTIFY_TEST=1)
# Default (no --no-daemon) runs a REAL isolated daemon, so the actual
# architecture — daemon-owned sessions, survival across a GUI restart — is what
# gets exercised, rather than routed around.
exec env "${env_args[@]}" "$BIN"
