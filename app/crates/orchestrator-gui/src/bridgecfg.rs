//! The non-visual half of Settings → Mobile: minting the token, reading the
//! daemon's endpoints back into the two access switches, and building the string
//! the QR code carries.
//!
//! Kept out of `settings.rs` because all of it is pure enough to test, and none of
//! it should have to be re-derived by a view that is busy laying out cards.
//!
//! Nothing here probes the network. `bridge_bind` names addresses SYMBOLICALLY
//! and the bridge resolves them at bind time, so there is no derived address to
//! cache and therefore no window anyone has to reopen after starting Tailscale or
//! changing networks.

use std::net::IpAddr;

/// The pairing payload. A CONTRACT shared with the iOS app's `Pairing.parse`;
/// changing the shape here breaks every phone already paired.
///
/// One string carries host, port, token and key together, which is the whole
/// point: the alternative is the user transcribing four fields into a phone, two
/// of which are long random strings typed into a masked box.
///
/// `f` is the base64url SHA-256 of the server's public key, and it is the ONLY
/// thing the phone uses to decide who answered — no CA will issue a certificate
/// for 192.168.0.71, so the certificate is self-signed and hostname matching
/// proves nothing. Present → the phone dials `wss://` and pins that key, and
/// pinning the KEY rather than the address is why a DHCP renewal or a new
/// Tailscale address breaks nothing. Absent → `ws://`, in the clear, which the
/// phone accepts only for its own loopback. So omitting `f` is not a smaller
/// code, it is a different promise: emit it only when there is genuinely no
/// certificate.
pub fn pair_url(host: &str, port: u16, token: &str, fingerprint: Option<&str>) -> String {
    match fingerprint {
        Some(f) => format!("kod://pair?h={host}&p={port}&t={token}&f={f}"),
        None => format!("kod://pair?h={host}&p={port}&t={token}"),
    }
}

/// The `bridge_bind` value for a pair of switches: `""`, `"lan"`, `"tailscale"`
/// or `"lan,tailscale"`.
///
/// SYMBOLIC, never an address. The bridge resolves these tokens at bind time, so
/// this Mac moving to another Wi-Fi, renewing its DHCP lease or restarting
/// Tailscale needs no cache invalidation and no re-probe here — which is exactly
/// the staleness the boot-time probe used to produce.
pub fn bind_tokens(lan: bool, tailnet: bool) -> String {
    match (lan, tailnet) {
        (false, false) => String::new(),
        (true, false) => "lan".to_string(),
        (false, true) => "tailscale".to_string(),
        (true, true) => "lan,tailscale".to_string(),
    }
}

/// Which switches a STORED bind implies. Used ONLY when nothing is listening,
/// because then there are no endpoints to read the real answer off.
///
/// A literal IPv4 stays legal — the CLI takes one, and it is how you pin a single
/// address — so one is classified by what it IS rather than ignored: a stored
/// 100.x address that drew both switches Off would invite a click that silently
/// narrowed the bind on the next start.
pub fn bind_switches(bind: &str) -> (bool, bool) {
    let (mut lan, mut tailnet) = (false, false);
    for tok in bind.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if tok.eq_ignore_ascii_case("lan") {
            lan = true;
        } else if tok.eq_ignore_ascii_case("tailscale") {
            tailnet = true;
        } else if let Ok(ip) = tok.parse::<IpAddr>() {
            match classify(ip) {
                Some(true) => tailnet = true,
                Some(false) => lan = true,
                None => {}
            }
        }
    }
    (lan, tailnet)
}

/// What the daemon has ACTUALLY bound, split the two ways the access switches are
/// drawn. `Some(addr)` is both "this switch is on" and the address to name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bound {
    pub lan: Option<String>,
    pub tailnet: Option<String>,
}

/// Classify the live endpoints. This is what keeps a switch from rendering
/// narrower than reality: a bind the daemon refused, or one it never received,
/// leaves the previous socket open, and a control drawn from the stored key would
/// show Off over a socket that is still accepting.
pub fn bound(endpoints: &[String]) -> Bound {
    let mut out = Bound::default();
    for host in hosts(endpoints) {
        let Ok(ip) = host.parse::<IpAddr>() else { continue };
        match classify(ip) {
            Some(true) => out.tailnet.get_or_insert_with(|| host.to_string()),
            Some(false) => out.lan.get_or_insert_with(|| host.to_string()),
            None => continue,
        };
    }
    out
}

/// `Some(true)` tailnet, `Some(false)` LAN, `None` loopback (which is bound
/// regardless and is not a switch).
///
/// Delegates to `ws::is_tailnet` rather than re-deriving 100.64.0.0/10 here: two
/// definitions of "is this a tailnet address" is one too many, and the one that
/// matters is the one the bridge enforces.
fn classify(ip: IpAddr) -> Option<bool> {
    if ip.is_loopback() {
        return None;
    }
    Some(orchestrator_bridge::ws::is_tailnet(ip))
}

/// 32 bytes from the OS CSPRNG, lowercase hex.
///
/// Returns `Err` rather than falling back to anything derived from the clock or
/// the pid. There is a `mint_epoch` in the bridge that does exactly that, and its
/// own doc says it is for uniqueness and NOT unpredictability — which is fine for
/// a cache key and disqualifying for a bearer token. A guessable credential here
/// is worse than no bridge at all, so a failure to read randomness must surface as
/// a visible error and leave the bridge off.
pub fn mint_token() -> Result<String, String> {
    // read_exact of a FIXED length, never `fs::read`: /dev/urandom is an endless
    // stream, so reading it "to the end" never returns.
    Ok(hex(&read_urandom(32)?))
}

fn read_urandom(n: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("could not open /dev/urandom: {e}"))?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)
        .map_err(|e| format!("could not read /dev/urandom: {e}"))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

/// The host a phone should dial, taken from what the daemon says it ACTUALLY bound
/// rather than from a stored preference.
///
/// This is the difference between a pairing card that works and one that hands out
/// an address nothing is listening on. A stored `bridge_bind` records what the user
/// asked for; `endpoints` records what the kernel gave. Only the second one can be
/// dialled.
pub fn reachable_host(endpoints: &[String]) -> Option<String> {
    hosts(endpoints)
        .find(|h| {
            h.parse::<IpAddr>()
                .map(|ip| !ip.is_loopback())
                .unwrap_or(false)
        })
        .map(|h| h.to_string())
}

/// The host half of each `addr:port` endpoint.
///
/// `rsplit_once` and the bracket trim are both load-bearing: an IPv6 endpoint is
/// `[fd7a:115c:a1e0::1]:8787`, so splitting at the FIRST colon yields `[fd7a` and
/// every caller downstream then fails to parse an address that is perfectly
/// dialable.
fn hosts(endpoints: &[String]) -> impl Iterator<Item = &str> {
    endpoints
        .iter()
        .filter_map(|e| e.rsplit_once(':').map(|(h, _)| h.trim_matches(['[', ']'])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_is_64_lowercase_hex_and_never_repeats() {
        let a = mint_token().expect("/dev/urandom must be readable on macOS");
        assert_eq!(a.len(), 64, "32 bytes, hex-encoded");
        assert!(
            a.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "must be lowercase hex — the phone's parser pins ^[0-9a-f]{{64}}$: {a}"
        );
        let b = mint_token().unwrap();
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn hex_pads_every_byte_to_two_digits() {
        // The bug this catches: a naive format!("{:x}") drops the leading zero on
        // bytes < 0x10, producing a token SHORTER than 64 chars that the phone's
        // length check then rejects — intermittently, only when a low byte is
        // drawn, which is the worst possible way to find out.
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn the_pairing_url_matches_the_contract_the_phone_parses() {
        let u = pair_url("100.101.102.103", 8787, &"a".repeat(64), None);
        assert!(u.starts_with("kod://pair?"));
        assert!(u.contains("h=100.101.102.103"));
        assert!(u.contains("p=8787"));
        assert!(u.ends_with(&format!("t={}", "a".repeat(64))));
        // No fingerprint means no `f`, and an ABSENT key is what tells the phone
        // this is a plaintext ws:// bridge. An empty `f=` would instead be a
        // fingerprint that matches nothing, so the phone would refuse a loopback
        // pairing that is legitimately unencrypted.
        assert!(!u.contains("f="), "a keyless code must not carry an f: {u}");
    }

    #[test]
    fn the_pairing_url_carries_the_key_to_pin_when_there_is_one() {
        // 43 chars is the real width: base64url of a SHA-256, unpadded.
        let f = "n4bQgYhMfWWaL_qgxVrQFaO_TxsrC4Is0V1sFbDwCgg";
        let u = pair_url("192.168.0.71", 8787, &"a".repeat(64), Some(f));
        assert!(u.ends_with(&format!("&f={f}")), "{u}");
        // The pin is the phone's ONLY notion of who answered, so it must survive
        // the trip: a code that dropped it silently downgrades to trusting
        // whatever holds that address.
        assert!(u.contains("h=192.168.0.71"));
        assert!(u.contains(&format!("t={}", "a".repeat(64))));
        // The encoder tops out at version 10 / 213 bytes (see `qr.rs`). Worst
        // case here is an IPv6 tailnet host, still ~173 — but a payload that
        // grows past the ceiling stops being a QR at all, so pin the headroom.
        assert!(u.len() < 200, "payload grew past what the encoder can carry: {}", u.len());
    }

    #[test]
    fn the_two_switches_map_onto_the_symbolic_bind_set() {
        // The exact strings the bridge parses. Written by the GUI, read by the
        // CLI: a typo here is a bind the daemon refuses with the switch showing
        // On, which is the whole failure this pane is built to avoid.
        assert_eq!(bind_tokens(false, false), "");
        assert_eq!(bind_tokens(true, false), "lan");
        assert_eq!(bind_tokens(false, true), "tailscale");
        assert_eq!(bind_tokens(true, true), "lan,tailscale");
    }

    #[test]
    fn every_switch_combination_survives_a_round_trip_through_the_stored_string() {
        // What breaks without this: the pane reads the switches back out of the
        // stored key whenever nothing is listening, so a value it can write but
        // not read renders as Off and the next click writes the OPPOSITE of what
        // the user sees.
        for lan in [false, true] {
            for tailnet in [false, true] {
                let s = bind_tokens(lan, tailnet);
                assert_eq!(bind_switches(&s), (lan, tailnet), "round trip failed for {s:?}");
            }
        }
    }

    #[test]
    fn a_literal_address_still_lights_the_switch_it_would_bind() {
        // A literal stays legal for the CLI and for pinning one address; the
        // switches have to place it, not ignore it.
        assert_eq!(bind_switches("100.101.102.103"), (false, true));
        assert_eq!(bind_switches("192.168.0.71"), (true, false));
        assert_eq!(bind_switches("fd7a:115c:a1e0::1"), (false, true));
        // Loopback is bound regardless, so it is not a switch.
        assert_eq!(bind_switches("127.0.0.1"), (false, false));
        assert_eq!(bind_switches("loopback"), (false, false));
        assert_eq!(bind_switches("  lan , tailscale  "), (true, true));
    }

    #[test]
    fn the_switches_are_read_off_the_addresses_actually_bound() {
        let eps = vec![
            "127.0.0.1:8787".to_string(),
            "192.168.0.71:8787".to_string(),
            "100.101.102.103:8787".to_string(),
        ];
        let b = bound(&eps);
        assert_eq!(b.lan.as_deref(), Some("192.168.0.71"));
        assert_eq!(b.tailnet.as_deref(), Some("100.101.102.103"));
    }

    #[test]
    fn a_loopback_only_bridge_leaves_both_switches_off() {
        // The property the radios had and the switches must keep: a control
        // derived from the live endpoints cannot draw an exposure that does not
        // exist. Loopback is bound whatever the user chose, so it is never a
        // switch — and no phone can reach it.
        assert_eq!(bound(&["127.0.0.1:8787".to_string()]), Bound::default());
        assert_eq!(bound(&[]), Bound::default());
    }

    #[test]
    fn the_dialable_host_is_the_non_loopback_endpoint() {
        let eps = vec!["127.0.0.1:8787".to_string(), "100.101.102.103:8787".to_string()];
        assert_eq!(reachable_host(&eps).as_deref(), Some("100.101.102.103"));
    }

    #[test]
    fn loopback_only_means_there_is_no_host_to_hand_out() {
        // The first-run trap: bound to loopback, a pairing card that printed
        // "127.0.0.1" would be naming the PHONE's own loopback once typed in. The
        // honest answer is that there is nothing to dial, so the UI must say so
        // instead of printing an address.
        assert_eq!(reachable_host(&["127.0.0.1:8787".to_string()]), None);
        assert_eq!(reachable_host(&[]), None);
    }

    #[test]
    fn an_ipv6_endpoint_is_unwrapped_rather_than_split_at_the_wrong_colon() {
        // rsplit_once(':') is right and split_once(':') would be wrong here.
        let eps = vec!["[fd7a:115c:a1e0::1]:8787".to_string()];
        assert_eq!(reachable_host(&eps).as_deref(), Some("fd7a:115c:a1e0::1"));
    }
}

/// Horizontal runs of dark modules, as `(x, y, len)`.
///
/// A 41×41 symbol is 1681 modules, and one element per module is a lot of layout
/// for a settings pane to redo on every repaint. Real QR rows coalesce to a
/// handful of runs, so this turns ~1681 elements into ~150 without changing a
/// pixel. Lives here, not in the view, because it is arithmetic and arithmetic
/// should be testable.
pub fn dark_runs(q: &crate::qr::Qr) -> Vec<(usize, usize, usize)> {
    let mut runs = Vec::new();
    for y in 0..q.size {
        let mut x = 0;
        while x < q.size {
            if !q.dark(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < q.size && q.dark(x, y) {
                x += 1;
            }
            runs.push((start, y, x - start));
        }
    }
    runs
}

#[cfg(test)]
mod run_tests {
    use super::{dark_runs, pair_url};
    use crate::qr::Qr;

    #[test]
    fn the_worst_case_pairing_payload_still_encodes() {
        // The fingerprint added ~46 bytes to a payload that used to land on
        // version 6, and this encoder stops at version 10 / 213 bytes. Worst
        // case is an IPv6 tailnet host with a 5-digit port; without this, the
        // first sign of overflowing the ceiling would be a settings pane that
        // says it could not build a pairing code.
        let url = pair_url(
            "fd7a:115c:a1e0:ab12:4843:cd96:6244:1a2b",
            65535,
            &"a".repeat(64),
            Some("n4bQgYhMfWWaL_qgxVrQFaO_TxsrC4Is0V1sFbDwCgg"),
        );
        let q = Qr::encode(&url).expect("the worst-case pairing payload must still encode");
        assert!(!dark_runs(&q).is_empty(), "an empty symbol is not a QR code");
    }

    #[test]
    fn runs_cover_exactly_the_dark_modules_and_nothing_else() {
        let q = Qr::encode("kod://pair?h=100.64.0.1&p=8787&t=abc").unwrap();
        let runs = dark_runs(&q);
        let covered: usize = runs.iter().map(|(_, _, w)| w).sum();
        let expected = (0..q.size)
            .flat_map(|y| (0..q.size).map(move |x| (x, y)))
            .filter(|(x, y)| q.dark(*x, *y))
            .count();
        assert_eq!(covered, expected, "runs must cover every dark module exactly once");
        for (x, y, w) in runs {
            for i in 0..w {
                assert!(q.dark(x + i, y), "run claims a light module at ({}, {y})", x + i);
            }
        }
    }

    #[test]
    fn coalescing_is_actually_worth_doing() {
        let q = Qr::encode("kod://pair?h=100.101.102.103&p=8787&t=deadbeef").unwrap();
        let modules = q.size * q.size;
        let runs = dark_runs(&q).len();
        println!("modules={modules} runs={runs} ratio={:.2}", modules as f32 / runs as f32);
        // Measured, not guessed: a real symbol of this size coalesces to roughly a
        // third of its module count, because QR data regions are deliberately
        // high-frequency. A 2x floor is the honest bar — it still turns ~1700
        // elements into ~700, and anything tighter is asserting a property of one
        // particular payload rather than of the optimisation.
        assert!(
            runs * 2 < modules,
            "runs={runs} vs modules={modules}: coalescing is not paying for itself"
        );
    }
}
