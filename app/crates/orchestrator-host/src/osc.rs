//! Raw-byte OSC tap (docs/013 §1 osc.rs).
//!
//! The pinned zed alacritty fork drops OSC 9 / 9;4 / 777 on the floor, but
//! those are confirmed-live WHEN-signals (docs/012 §1.1–1.2): codex announces
//! "Approval requested: …" via OSC 9 the instant an approval blocks; claude
//! emits OSC 9;4 busy/done progress and OSC 777 "waiting for your input".
//! So the host scans the raw byte stream *before* `proc.advance` and emits
//! typed signals. Stateful: sequences split across read chunks are buffered.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OscSignal {
    /// OSC 9 desktop notification (codex: "Approval requested: <cmd>",
    /// "Codex wants to edit N files", "Agent turn complete").
    Notify9(String),
    /// OSC 9;4 progress (ConEmu): state 0=done 1=set 2=error 3=indeterminate.
    Progress { state: u8, value: Option<u8> },
    /// OSC 777;notify;<title>;<body> (claude: "Claude is waiting for your input").
    Notify777 { title: String, body: String },
    /// OSC 0/2 window title (claude: spinner+activity while busy, "✳ …" idle).
    Title(String),
}

enum State {
    Ground,
    Esc,
    Osc(Vec<u8>),
    /// Inside an OSC payload and saw ESC — next byte `\` terminates (ST).
    OscEsc(Vec<u8>),
}

/// Stateful scanner. Feed every PTY chunk through `scan` (cheap: one pass,
/// no allocation outside active sequences).
pub struct OscTap {
    state: State,
}

impl Default for OscTap {
    fn default() -> Self {
        Self::new()
    }
}

impl OscTap {
    pub fn new() -> Self {
        OscTap { state: State::Ground }
    }

    pub fn scan(&mut self, chunk: &[u8]) -> Vec<OscSignal> {
        let mut out = Vec::new();
        for &b in chunk {
            self.state = match std::mem::replace(&mut self.state, State::Ground) {
                State::Ground => {
                    if b == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                }
                State::Esc => {
                    if b == b']' {
                        State::Osc(Vec::new())
                    } else {
                        State::Ground
                    }
                }
                State::Osc(mut buf) => {
                    match b {
                        0x07 => {
                            // BEL terminator
                            if let Some(sig) = parse_osc(&buf) {
                                out.push(sig);
                            }
                            State::Ground
                        }
                        0x1b => State::OscEsc(buf),
                        _ => {
                            // OSC payloads are bounded in practice; cap defensively
                            if buf.len() < 8192 {
                                buf.push(b);
                            }
                            State::Osc(buf)
                        }
                    }
                }
                State::OscEsc(buf) => {
                    if b == b'\\' {
                        // ST terminator
                        if let Some(sig) = parse_osc(&buf) {
                            out.push(sig);
                        }
                    }
                    State::Ground
                }
            };
        }
        out
    }
}

fn parse_osc(payload: &[u8]) -> Option<OscSignal> {
    let s = String::from_utf8_lossy(payload);
    let mut parts = s.splitn(2, ';');
    let code = parts.next()?;
    let rest = parts.next().unwrap_or("");
    match code {
        "0" | "2" => Some(OscSignal::Title(rest.to_string())),
        "9" => {
            // distinguish 9;4;<state>[;<value>] progress from plain 9;<text>
            let mut segs = rest.split(';');
            let first = segs.next().unwrap_or("");
            if first == "4" {
                let state = segs.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                let value = segs.next().and_then(|x| x.parse().ok());
                Some(OscSignal::Progress { state, value })
            } else {
                Some(OscSignal::Notify9(rest.to_string()))
            }
        }
        "777" => {
            let mut segs = rest.splitn(3, ';');
            let kind = segs.next().unwrap_or("");
            if kind != "notify" {
                return None;
            }
            Some(OscSignal::Notify777 {
                title: segs.next().unwrap_or("").to_string(),
                body: segs.next().unwrap_or("").to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn codex_approval_notification_parses() {
        // exact form captured live (docs/012 §1.2)
        let mut tap = OscTap::new();
        let sigs =
            tap.scan(b"\x1b]9;Approval requested: /bin/zsh -lc 'touch marker.txt'\x07");
        assert_eq!(sigs.len(), 1);
        match &sigs[0] {
            OscSignal::Notify9(t) => assert!(t.starts_with("Approval requested:")),
            other => panic!("wrong signal: {other:?}"),
        }
    }

    #[test]
    fn progress_and_notify777_parse() {
        let mut tap = OscTap::new();
        let sigs = tap.scan(
            b"\x1b]9;4;3\x07\x1b]9;4;0\x07\x1b]777;notify;Claude Code;Claude is waiting for your input\x07",
        );
        assert_eq!(
            sigs[0],
            OscSignal::Progress { state: 3, value: None }
        );
        assert_eq!(sigs[1], OscSignal::Progress { state: 0, value: None });
        assert_eq!(
            sigs[2],
            OscSignal::Notify777 {
                title: "Claude Code".into(),
                body: "Claude is waiting for your input".into()
            }
        );
    }

    #[test]
    fn sequence_split_across_chunks_survives() {
        let mut tap = OscTap::new();
        let whole = b"\x1b]9;Approval requested: touch x\x07";
        let (a, b) = whole.split_at(12);
        assert!(tap.scan(a).is_empty());
        let sigs = tap.scan(b);
        assert_eq!(sigs.len(), 1);
    }

    #[test]
    fn st_terminator_accepted() {
        let mut tap = OscTap::new();
        let sigs = tap.scan(b"\x1b]2;hello title\x1b\\");
        assert_eq!(sigs, vec![OscSignal::Title("hello title".into())]);
    }

    #[test]
    fn live_claude_stream_yields_titles_progress_and_waiting() {
        // claude_raw2.log: real hosted claude session (Bash dialog answered).
        // Probed: 11 OSC-0 titles, 3 OSC-9;4 progress, 1 OSC-777.
        let bytes = fixture("claude/2.1.172/pty/claude_raw2.log");
        let mut tap = OscTap::new();
        let sigs = tap.scan(&bytes);
        let titles = sigs.iter().filter(|s| matches!(s, OscSignal::Title(_))).count();
        let progress = sigs.iter().filter(|s| matches!(s, OscSignal::Progress { .. })).count();
        let waiting = sigs
            .iter()
            .filter(|s| matches!(s, OscSignal::Notify777 { .. }))
            .count();
        assert!(titles >= 1, "expected titles, got {sigs:?}");
        assert_eq!(progress, 3);
        assert_eq!(waiting, 1);
    }
}
