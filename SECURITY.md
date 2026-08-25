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

**The mobile bridge and its token.** Kod can serve a read-only view of your
session list to a phone (Settings → Mobile). It is **off by default** and does
nothing until you turn it on. When you do, Kod mints a 32-byte random bearer
token — this is the first credential Kod creates *on your behalf* rather than one
you typed, and it is stored **in plaintext** in the same local store described
below. Anyone who can read that file, or read the token off your screen, can read
every project name, session title and last-message line until you regenerate it.

The listener always binds loopback, and additionally binds **one** Tailscale
address (100.64.0.0/10) if you choose that. It refuses every other bind — LAN
addresses and `0.0.0.0` included — because this version authenticates with a
shared bearer token and has **no TLS**, so any other interface would put that
token in the clear. Over Tailscale, WireGuard already authenticates the device and
encrypts the hop. The phone is a **reader**: the protocol carries no input, and
the server cannot be asked to type into a session.

One consequence worth stating plainly: the bridge is hosted by Kod's session
daemon, which outlives the app window. **Closing or quitting Kod does not stop it**
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
