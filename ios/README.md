# Kod Remote (iOS)

A mini Kod for the phone: **Standup · Projects · Session**. It answers "does
anything need me?" — it is not a terminal. `caps.input` is `false` in v0 and the
Session tab is a reader, because an 80x24 grid on a 390pt screen is unreadable
and a PTY is the wrong thing to put behind a lock screen.

## Shape

    iPhone ──ws (Tailscale/WireGuard)──▶ kod-bridge ──unix socket──▶ orchestrator-daemon

The bridge is an ordinary daemon **client**. It depends on `orchestrator-host` for
the protocol types so it cannot announce a wire version the daemon does not
expect, and it mirrors sessions rather than proxying the terminal: grid frames
and timeline events are dropped, since the phone renders neither.

## Connecting your phone

1. Get the Mac's tailnet address:

       tailscale ip -4

2. Start the bridge, bound to loopback **and** that address:

       KOD_BRIDGE_TOKEN=$(openssl rand -hex 24) \\
       KOD_BRIDGE_BIND=$(tailscale ip -4 | head -1) \\
       cargo run -p orchestrator-bridge --bin kod-bridge -- serve <daemon.sock>

   `KOD_BRIDGE_BIND` accepts **only** loopback or a tailnet address
   (100.64.0.0/10). A LAN or wildcard bind is refused: v0 has one shared bearer
   token and no TLS, so those would put the token in the clear. Over Tailscale
   WireGuard already authenticates the device and encrypts the hop.

   There is **no default token**, deliberately — a default token is a published
   token.

3. In the app, tap the connection chip and enter host, port (default
   `8787`, matching the bridge) and the token. The token is stored in the
   Keychain.

### Which socket?

Name it explicitly. `kod-bridge` refuses your default daemon socket unless
`KOD_BRIDGE_ALLOW_DEFAULT` is set, because attaching a freshly-built client can
**retire** a running daemon and kill every live agent session. For a sandbox:

    ./app/scripts/dev-sandbox.sh --snapshot     # real store, isolated daemon

## Tests

    xcodebuild test -project ios/Kod.xcodeproj -scheme Kod        # pure, hermetic
    xcodebuild test -project ios/Kod.xcodeproj -scheme KodUITests # needs a live bridge

`KodUITests` drives the shipped app against a real bridge and **skips loudly**
when there isn't one — it exists to catch the failures that live in the seam
between two binaries, which nothing pure can reach. The port mismatch it was
written after (app defaulted to 8765, bridge bound 8787) is now pinned from both
sides: `BridgeSettings.defaultPort` and `ws::DEFAULT_PORT`.

## Project file

`ios/Kod.xcodeproj` is generated and gitignored. Edit `ios/project.yml`, then:

    xcodegen generate
