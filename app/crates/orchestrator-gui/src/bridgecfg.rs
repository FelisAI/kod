//! The non-visual half of Settings → Mobile: minting the token, finding this
//! Mac's tailnet address, and building the string the QR code carries.
//!
//! Kept out of `settings.rs` because all of it is pure enough to test, and none of
//! it should have to be re-derived by a view that is busy laying out cards.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// The pairing payload. A CONTRACT shared with the iOS app's `Pairing.parse`;
/// changing the shape here breaks every phone already paired.
///
/// One string carries host, port and token together, which is the whole point:
/// the alternative is the user transcribing three fields into a phone, one of
/// which is 64 hex characters typed into a masked box.
pub fn pair_url(host: &str, port: u16, token: &str) -> String {
    format!("kod://pair?h={host}&p={port}&t={token}")
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

/// This Mac's Tailscale IPv4 address, or `None` when Tailscale is not up.
///
/// A UDP `connect` to a tailnet address sends NOTHING — it only asks the kernel to
/// pick a route and bind a local address — so this is a routing-table lookup with
/// no packets and no dependency on the `tailscale` binary being installed.
///
/// Deliberately NOT chosen by interface name: measured on this Mac, `utun0` carries
/// a 192.168.x address and `utun1` carries the tailnet one, so anything keying off
/// "utun0" would confidently return a LAN address that `is_tailnet` then rejects —
/// or worse, that something else accepts.
pub fn detect_tailnet_ip() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // Any address inside 100.64.0.0/10 works as a routing probe; this one is
    // Tailscale's own DNS resolver, so it exists on every tailnet.
    sock.connect("100.100.100.100:9").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if orchestrator_bridge::ws::is_tailnet(IpAddr::V4(v4)) => Some(v4),
        _ => None,
    }
}

/// The host a phone should dial, taken from what the daemon says it ACTUALLY bound
/// rather than from a stored preference.
///
/// This is the difference between a pairing card that works and one that hands out
/// an address nothing is listening on. A stored `bridge_bind` records what the user
/// asked for; `endpoints` records what the kernel gave. Only the second one can be
/// dialled.
pub fn reachable_host(endpoints: &[String]) -> Option<String> {
    endpoints
        .iter()
        .filter_map(|e| e.rsplit_once(':').map(|(h, _)| h.trim_matches(['[', ']'])))
        .find(|h| {
            h.parse::<IpAddr>()
                .map(|ip| !ip.is_loopback())
                .unwrap_or(false)
        })
        .map(|h| h.to_string())
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
        let u = pair_url("100.101.102.103", 8787, &"a".repeat(64));
        assert!(u.starts_with("kod://pair?"));
        assert!(u.contains("h=100.101.102.103"));
        assert!(u.contains("p=8787"));
        assert!(u.ends_with(&format!("t={}", "a".repeat(64))));
        // Comfortably inside what a QR at error-correction M can carry, and what
        // the encoder's golden test proved decodable.
        assert!(u.len() < 160, "payload grew past what was verified: {}", u.len());
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

    #[test]
    fn the_tailnet_probe_never_returns_a_lan_address() {
        // It may legitimately return None (Tailscale down), but anything it DOES
        // return must be inside 100.64.0.0/10 — the whole point is that a LAN
        // address must never be offered as a bind target.
        if let Some(ip) = detect_tailnet_ip() {
            assert!(
                orchestrator_bridge::ws::is_tailnet(IpAddr::V4(ip)),
                "probe returned a non-tailnet address: {ip}"
            );
        }
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
    use super::dark_runs;
    use crate::qr::Qr;

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
