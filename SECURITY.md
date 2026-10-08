# Security Policy

Kod is a native macOS app that runs on your machine, under your user account. It
has no Kod-operated server: nothing is sent to FelisAI, and the only network
traffic a default build makes is your own agent CLIs talking to their providers
(plus Standup's summaries, which go through **your** LLM account). This shapes
the threat model below.

## Supported versions

Kod is pre-1.0. Only the **latest `0.x` release** receives security fixes; there
are no backports to older tags. Track `main` / the newest release.

## Reporting a vulnerability

Please report privately — **do not** open a public issue for a security bug.

Open a **[GitHub private security advisory](https://github.com/FelisAI/kod/security/advisories/new)**
on `FelisAI/kod` (the repo's **Security → Report a vulnerability** form). That's the
private channel the maintainer monitors — this is an open-source project, so there
is no security email.

Include what you'd need to reproduce it: affected version/commit, your macOS
version, steps, and impact. A proof-of-concept helps.

### What to expect

- **Acknowledgement** within **5 business days**.
- An **initial assessment** (severity + whether we can reproduce) within **10
  business days**.
- Coordinated disclosure: we'll agree on a timeline with you and credit you in
  the advisory unless you'd rather stay anonymous.

This is a small project — if you haven't heard back within the acknowledgement
window, please add a comment to nudge the advisory.

## Threat model & attack surface

Everything in Kod runs locally as **you**. The trust boundary is your macOS user
account — Kod adds no privilege boundary of its own, and anything already running
as your user can do what Kod does. The surfaces worth knowing about:

**The local daemon control socket.** Kod owns your sessions through a long-lived
daemon reached over a Unix domain socket at
`$XDG_RUNTIME_DIR` (or `$TMPDIR`) `/orchestrator/daemon.sock`. This is a local
automation surface, not a network service: the socket's directory is created
**`0700`** (owner-only), which is the boundary. There is **no auth token** on the
socket itself — any process that can reach it can drive the daemon, and the
daemon's commands include spawning shells and CLIs (`SpawnShell`, `Spawn`) and
injecting keystrokes into any session (`SendKey`). Treat socket access as
equivalent to shell access as your user.

**Child CLIs and per-profile credentials.** Kod spawns `claude`, `codex`, and
shells as child processes. Kod does **not** hold your Anthropic/OpenAI
credentials; account isolation is done by injecting environment variables that
point each CLI at its own config home — `CLAUDE_CONFIG_DIR` for `claude`,
`CODEX_HOME` for `codex` — plus a per-profile `env` map that layers on top. Those
config homes (and the credentials in them) are owned and secured by the CLIs
themselves. Note that any extra environment values you enter on a **profile** are
stored **in plaintext** in the local store (see below), not the macOS Keychain —
so don't put long-lived secrets in a profile's env if your disk or backups aren't
trusted.

**The mobile bridge and its token.** Kod can serve your sessions to the Kod Remote
iPhone app, and let you answer them from it (Settings → Mobile). It is **off by
default** and does nothing until you turn it on. When you do, Kod mints a 32-byte
random bearer token — this is the first credential Kod creates *on your behalf*
rather than one you typed, and it is stored **in plaintext** in the same local store
described below. The pairing code (and the "Copy pairing link" link) carries it.

**Treat the token like a password to a shell on your Mac.** A phone holding it can
see every project name, session title and last message, **and type into any live
session — a `claude`, a `codex` or a plain shell — and press a fixed set of twenty
keys, Ctrl-C among them.** Typing into a shell is running commands as you, so
whoever holds the token can do that too. If a pairing code or link may have been
seen by someone else, use **Regenerate token** in Settings → Mobile: it signs out
every paired phone at once.

The listener always binds loopback. It additionally binds your Wi-Fi network address
and/or your Tailscale address (100.64.0.0/10) only if you turn those on — a "Wi-Fi"
address must sit on a physical interface, so a VPN tunnel is never offered as one —
and it refuses `0.0.0.0` always. **Anything beyond loopback is TLS-only:** the Mac
serves a self-signed certificate, and the pairing code carries the SHA-256 of its
public key, which the phone pins — it refuses any other key, and it refuses to send
the token in the clear to anything but its own loopback. The key, not the address,
identifies your Mac, so the Mac's addresses may change without re-pairing.

What the phone may do is enforced on the Mac, not by the phone. The bridge runs as a
**separate process** that attaches to the session daemon holding a restricted
capability: it may send typed text and the twenty named keys, and is refused every
other command outright — spawning, closing, and the arbitrary-keystroke path the
desktop uses included. The phone sends only a session id; the daemon resolves the
session from its own state and refuses ended sessions and ids it does not know. Text
from a phone is capped at 8 KiB and stripped of control characters before it reaches
a terminal, so an escape sequence cannot be smuggled inside a message — the named
keys are the only way to send one. The bridge accepts at most eight connections and
drops one that has been silent for a minute.

One consequence worth stating plainly: the bridge is a helper process started by
Kod's session daemon, which outlives the app window. **Closing or quitting Kod
does not stop it**
— that is deliberate, since the point is checking your sessions while you are away
from the Mac, but it means the listener keeps running until you turn it off or the
daemon exits.

**Local session data.** Kod keeps its own state in a SQLite database at
`~/Library/Application Support/orchestrator/store.db` — projects, session records,
profiles (including the plaintext profile `env` above), the mobile-bridge token
if you enabled it, and an activity log. Kod tightens this directory to `0700` and
the database and its `-wal`/`-shm` sidecars to `0600` every time it opens them —
on older installs these were created world-readable, so the fix is applied on
every launch rather than only at creation. To
build summaries and to recover/resume sessions, Kod **reads** the agent CLIs'
transcripts from their own homes (`~/.claude`, `~/.codex`, or a profile's config
dir). This data stays on your machine; the only content that leaves is what
Standup sends to your own LLM account to summarize a session.

## Distribution & binary integrity

v0 is **source-only**: you build Kod from source (see the README). There is **no
signed or notarized binary yet**, so verify what you build. A locally built
`Kod.app` carries no Gatekeeper quarantine — that's expected for a from-source
build and is the correct bar for v0. Signed, notarized releases are a
post-v0 concern.
