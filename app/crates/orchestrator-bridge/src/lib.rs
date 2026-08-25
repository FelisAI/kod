//! orchestrator-bridge — the process that lets a phone see and drive Kod.
//!
//! It is an ORDINARY DAEMON CLIENT on one side (the same unix socket and the
//! same bincode protocol the desktop GUI speaks) and, later, a WebSocket server
//! for the iOS app on the other.
//!
//! Slice 0 was this half only: attach, mirror what the daemon says, and be able
//! to prove it — the sequencing rule being that the bridge must be correct
//! against a throwaway client before an app exists, so that when the app
//! misbehaves the server is not a suspect.
//!
//! Slice 1 adds the other half: [`wire`] is the phone-facing JSON protocol and
//! [`ws`] is the blocking WebSocket server that speaks it. Both are structured
//! so the protocol, the auth and the fan-out are pure and unit-tested with
//! nothing connected — in this crate the cheapest-looking integration test is
//! one that attaches to a real daemon, and a wrong attach RETIRES it (see
//! [`client`]).
//!
//! The phone can watch every session, and can TYPE into claude and codex
//! sessions — never into a shell. That last rule is not enforced here and must
//! not be: this process is the one a network peer talks to, so it attaches as
//! [`orchestrator_host::protocol::ClientRole::Phone`] and the daemon refuses it
//! anything else, resolving each session's kind from its own state. Nothing in
//! this crate can grant a capability; it can only ask, and be told no.

pub mod client;
pub mod mirror;
pub mod wire;
pub mod ws;
