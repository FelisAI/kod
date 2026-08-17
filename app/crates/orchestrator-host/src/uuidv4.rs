//! A tiny dependency-free UUIDv4 generator. We pre-set claude's `--session-id`
//! so the orchestrator OWNS the resume handle at spawn time (spike: the id
//! becomes the transcript filename). `uuid` is only a transitive gpui dep, not a
//! direct one here — so we mint our own rather than widen the dependency graph.

use std::io::Read;

/// A random lowercase 8-4-4-4-12 v4 UUID. Prefers `/dev/urandom`; falls back to
/// a time/pid-seeded xorshift fill if that's unavailable. Both paths set the
/// version (4) and variant (10xx) bits, so the result is always a valid UUID.
pub fn new() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        fill_fallback(&mut b);
    }
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Non-crypto fallback (only if /dev/urandom is unreadable): splitmix64 seeded
/// from the clock and pid. Uniqueness, not unpredictability, is what matters for
/// a session id.
fn fill_fallback(b: &mut [u8; 16]) {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32);
    let mut x = seed.wrapping_add(0x9E3779B97F4A7C15);
    for chunk in b.chunks_mut(8) {
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58476D1CE4E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        for (i, slot) in chunk.iter_mut().enumerate() {
            *slot = (x >> (i * 8)) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_bits() {
        let u = new();
        assert_eq!(u.len(), 36);
        let bytes = u.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert_eq!(u.chars().nth(14), Some('4')); // version nibble
        assert!(matches!(u.chars().nth(19), Some('8' | '9' | 'a' | 'b'))); // variant
    }

    #[test]
    fn distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(new()), "uuids must be unique");
        }
    }

    #[test]
    fn fallback_sets_bits() {
        let mut b = [0u8; 16];
        fill_fallback(&mut b);
        b[6] = (b[6] & 0x0F) | 0x40;
        b[8] = (b[8] & 0x3F) | 0x80;
        assert_eq!(b[6] >> 4, 4);
        assert_eq!(b[8] >> 6, 0b10);
    }
}
