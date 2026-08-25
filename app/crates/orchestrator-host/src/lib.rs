//! orchestrator-host — the terminal-hosting substrate (docs/013 §1).
//!
//! Owns PTYs, the per-session emulator, the OSC tap, and (next) the hook
//! ingress + Codex app-server client. The GUI is a consumer: it renders
//! snapshots and calls verbs; it never sees alacritty/portable-pty types.
//!
//! API hygiene rule (daemon-extraction insurance, docs/013 §1): every public
//! type is plain `serde` data — enforced by `public_api_is_plain_data` below.

pub mod color;
pub mod decision;
pub mod emulator;
pub mod events;
pub mod grid_view;
pub mod hooks;
pub mod host;
pub mod ingress;
pub mod input;
pub mod osc;
pub mod protocol;
pub mod pty;
pub mod session;
pub mod transcript;
pub mod usage_limit;
mod util;
mod uuidv4;

pub use decision::{DecisionView, PendingDecision};
pub use events::{event_line, EventLine, EventTone, SessionEvent, SessionEventKind, ToolVerb};
pub use host::{SessionBackend, SessionHost, SessionInfo};
pub use session::{CliKind, HostedSession, Phase, SessionId, Trouble, TroubleKind, UsageLimit};

#[cfg(test)]
mod api_hygiene {
    fn assert_plain<T: serde::Serialize + serde::de::DeserializeOwned>() {}

    #[test]
    fn public_api_is_plain_data() {
        assert_plain::<crate::input::KeyInput>();
        assert_plain::<crate::input::ModeFlags>();
        assert_plain::<crate::osc::OscSignal>();
        assert_plain::<crate::hooks::HookEvent>();
        assert_plain::<crate::emulator::GridSnapshot>();
        assert_plain::<crate::emulator::StyleRun>();
        assert_plain::<crate::emulator::EmuEvent>();
        assert_plain::<crate::session::SessionId>();
        assert_plain::<crate::session::CliKind>();
        assert_plain::<crate::session::Phase>();
        assert_plain::<crate::host::SessionInfo>();
        assert_plain::<crate::pty::SpawnSpec>();
        assert_plain::<crate::decision::PendingDecision>();
        assert_plain::<crate::decision::DecisionView>();
        assert_plain::<crate::events::SessionEvent>();
        assert_plain::<crate::events::SessionEventKind>();
        assert_plain::<crate::protocol::BridgeStatus>();
    }
}

#[cfg(test)]
mod test_box_drawing {
    use super::grid_view::detect_urls;

    #[test]
    fn test_box_drawing_false_positive() {
        // Box-drawing char immediately after domain should be included in match
        let result = detect_urls("https://example.com─────");
        println!("\nBox drawing test:");
        println!("  Input: 'https://example.com─────'");
        println!("  Result: {:?}", result);
        assert!(!result.is_empty());
        // The URL string WILL include the box-drawing chars (FALSE POSITIVE)
        let url = &result[0].2;
        println!("  Matched URL: '{}'", url);
        assert!(url.contains('─'), "Box-drawing char included in match");
    }

    #[test]
    fn test_space_stops_correctly() {
        // Space should stop the URL
        let result = detect_urls("https://example.com ");
        println!("\nSpace test:");
        println!("  Input: 'https://example.com '");
        println!("  Result: {:?}", result);
        let url = &result[0].2;
        println!("  Matched URL: '{}'", url);
        assert_eq!(url, "https://example.com");
    }
}
