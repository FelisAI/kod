# Kod Remote (iOS)

A mini Kod for the phone: **Standup · Projects · Session**. It answers "does anything need me?", and lets you answer back.

The Session tab shows the session's terminal at its real width (scroll sideways),
its last message, and a composer: type into any live session — claude, codex or a
shell — and press the keys a prompt or a command line needs (enter, escape, arrows,
tab, Ctrl-C and friends). The phone cannot spawn or close anything. What it may do is
enforced on the Mac, by the daemon; see SECURITY.md.

## Shape

    iPhone ──wss (Wi-Fi or Tailscale, pinned key)──▶ kod-bridge ──unix socket──▶ orchestrator-daemon

The bridge is an ordinary daemon **client**, started and supervised by the daemon
when you turn on Settings → Mobile. It depends on `orchestrator-host` for the
protocol types so it cannot announce a wire version the daemon does not expect, and
it mirrors sessions rather than proxying the terminal: a terminal is streamed only
for the one session a phone is watching.

## Pairing your phone

1. On the Mac: Kod → Settings → Mobile → turn on **Serve my sessions to my phone**,
   and choose **My Wi-Fi network** and/or **My Tailscale network**.
2. On the phone: tap **Pair** (or the connection chip) and **Scan QR code** — or use
   **Copy pairing link** on the Mac and **Paste** on the phone (Universal Clipboard
   carries it across).

The code carries every address the Mac answers at, the access token, and the
SHA-256 of the Mac's TLS public key. The phone pins that key and refuses any other;
the token goes in the Keychain. There is no default token — a default token is a
published token.

**No Mac nearby?** "Explore with sample data" (on the home screen and the connection
sheet) fills the app with made-up sessions and answers typing itself, behind a
banner that says so. It is how App Review sees the app.

## Running a bridge by hand (development)

Name the daemon socket explicitly. `kod-bridge` refuses your default daemon socket
unless `KOD_BRIDGE_ALLOW_DEFAULT` is set, because attaching a freshly-built client
can **retire** a running daemon and kill every live agent session. Use a sandbox:

    ./app/scripts/dev-sandbox.sh --snapshot     # real store, isolated daemon

or a throwaway daemon of your own: run `orchestrator-daemon` with
`XDG_RUNTIME_DIR` pointed somewhere short and private (a unix socket path must stay
under ~104 bytes), seed it with `cargo run -p orchestrator-bridge --example
seed_sessions -- <that socket>` (it refuses any path without `koddemo` in it), then

    KOD_BRIDGE_TOKEN=$(openssl rand -hex 32) KOD_BRIDGE_PORT=28787 \
      cargo run -p orchestrator-bridge --bin kod-bridge -- serve <that socket>

The banner prints the pin. The pairing link is
`kod://pair?h=127.0.0.1&p=28787&t=<token>&f=<pin>`.

## Tests

    xcodebuild test -project ios/Kod.xcodeproj -scheme Kod          # pure, hermetic
    TEST_RUNNER_KOD_PAIR_LINK='kod://pair?…' \
      xcodebuild test -project ios/Kod.xcodeproj -scheme KodUITests  # needs a SANDBOX bridge

`KodTests` is pure: the wire, the cache, the plans, pairing, pinning, the composer,
the watch, the sample data. `KodUITests` drives the shipped app:
`ConnectionSheetUITests` needs nothing; `EndToEndUITests` pairs by pasting the link,
reads sessions, opens one, sees its terminal, types into a shell, interrupts it and
survives a trip to the background — against a real daemon and bridge, so it skips
loudly without `TEST_RUNNER_KOD_PAIR_LINK`. `FrozenLinkUITests` is opt-in
(`TEST_RUNNER_KOD_FREEZE_TEST=1`): a harness `kill -STOP`s the sandbox bridge once the
app is connected, and the phone must notice within its idle deadline.

Never point any of them at the daemon hosting your real sessions.

## Project file

`ios/Kod.xcodeproj` is generated and gitignored. Edit `ios/project.yml`, then:

    xcodegen generate
