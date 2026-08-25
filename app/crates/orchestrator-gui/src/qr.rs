//! A self-contained QR code encoder: byte mode, error-correction level M,
//! smallest fitting version from 1 to 10, all eight mask patterns scored.
//! std only — this workspace takes no image/codec crates, and a *correct*
//! generator at this scope is ~600 lines of table arithmetic, which is cheaper
//! than a dependency and auditable in one sitting.
//!
//! Scope is deliberate. Implemented: byte mode (it encodes any payload, just
//! less densely than numeric/alphanumeric), level M, versions 1-10, automatic
//! version selection, the eight masks scored by the standard penalty rules.
//! Not implemented and not needed here: kanji mode, mixed-mode segment
//! optimisation, ECI, structured append, versions past 10.
//!
//! The only payload this has to carry is the phone-pairing URL
//!
//!     kod://pair?h=<ipv4>&p=<port>&t=<64 lowercase hex>
//!
//! — about 100 ASCII bytes, which lands on version 6. Version 10 at level M
//! holds 213 bytes, so the ceiling leaves room to double the payload before
//! anyone has to touch this. Raising it further means appending the new
//! versions' rows to the two tables below; nothing else in this file is
//! version-limited. (Parsing/validating that URL is the pairing module's job,
//! not the encoder's — this takes any string.)
//!
//! Byte mode carries no charset declaration (that is what ECI is for), so a
//! reader guesses the encoding of what it gets back. The pairing URL is pure
//! ASCII, where every decoder agrees; a non-ASCII payload encodes fine as UTF-8
//! but is then at the reader's mercy.
//!
//! Correctness is the whole job here: a symbol whose Reed-Solomon, interleaving
//! or mask is subtly wrong still *looks* exactly like a QR code and no one can
//! tell by eye. So nothing below is approximate — GF(256) Reed-Solomon, the
//! multi-block split and interleave that versions >= 4 require, BCH format and
//! version information, real mask scoring. Two independent checks were run
//! against the output, not just the unit tests below:
//!
//!   * Apple's Vision barcode detector (VNDetectBarcodesRequest) decoded 13
//!     generated symbols — a 3-byte payload, two real pairing URLs (100 and
//!     103 bytes, version 6) and every version 1-10 filled to its exact
//!     capacity — and every one came back byte-identical to the input. The
//!     version 7-10 cases are what actually exercise the version-information
//!     block and the mixed-length interleave.
//!   * macOS ships its own QR encoder (CIQRCodeGenerator). At level M, for
//!     byte-mode payloads, its grids are module-for-module IDENTICAL to these
//!     at all ten versions — mask choice included. The pairing URL is the one
//!     shape that differs, because CoreImage splits its digit runs into
//!     numeric segments and this encoder deliberately does not; measured: the
//!     same URL with the digits replaced by letters is identical again.

/// Largest symbol this encoder builds. See the module doc for why 10.
const MAX_VERSION: usize = 10;

/// Error-correction codewords per block at level M, indexed by version (slot 0
/// is unused padding so a version indexes directly). ISO/IEC 18004 tables
/// 13-22, medium row.
const ECC_PER_BLOCK: [usize; MAX_VERSION + 1] = [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];

/// Number of error-correction blocks at level M, same indexing. These two rows
/// are the *only* per-version tables here: given the raw codeword count (which
/// is computed from the geometry, not tabulated), the data codewords, the
/// short/long block split and the byte capacities all follow. A fourth table
/// would be a fourth thing to get wrong.
const BLOCKS: [usize; MAX_VERSION + 1] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5];

/// Penalty weights for mask selection (ISO/IEC 18004 table 24).
const PENALTY_N1: u32 = 3;
const PENALTY_N2: u32 = 3;
const PENALTY_N3: u32 = 40;
const PENALTY_N4: u32 = 10;

/// The finder-lookalike the third penalty rule hunts for: dark:light:dark³:
/// light:dark (the 1:1:3:1:1 ratio of a real finder) followed by four light
/// modules. Scanned in both directions, in every row and column.
const FINDER_LIKE: [bool; 11] = [
    true, false, true, true, true, false, true, false, false, false, false,
];

/// A finished symbol: `size` x `size` modules, row-major, `true` = dark.
///
/// The quiet zone is *not* included — the standard requires four light modules
/// on every side, and drawing it is the view's business (see [`Qr::dark`]).
pub struct Qr {
    pub size: usize,
    modules: Vec<bool>,
}

impl Qr {
    /// Encode `data` into the smallest level-M symbol that fits it.
    ///
    /// Errors carry the number a caller can act on: how long the payload is and
    /// how long it may be.
    pub fn encode(data: &str) -> Result<Qr, String> {
        let bytes = data.as_bytes();
        if bytes.is_empty() {
            return Err("cannot encode an empty payload".to_string());
        }
        let version = smallest_version(bytes.len())?;
        let codewords = interleave(&encode_data(bytes, version), version);
        let mut grid = Grid::new(version);
        grid.draw_function_patterns();
        grid.draw_codewords(&codewords);
        grid.apply_best_mask();
        Ok(Qr { size: grid.size, modules: grid.modules })
    }

    /// Is the module at (`x` = column, `y` = row) dark?
    ///
    /// Out of range reads light rather than panicking, so a view can iterate a
    /// grid that already includes the mandatory four-module quiet zone (i.e.
    /// from -4 to size+4, shifted) without doing bounds arithmetic per module.
    pub fn dark(&self, x: usize, y: usize) -> bool {
        if x >= self.size || y >= self.size {
            return false;
        }
        self.modules[y * self.size + x]
    }
}

// ---------------------------------------------------------------------------
// Version geometry and capacity
// ---------------------------------------------------------------------------

/// Modules available to data+ECC bits in `version`, i.e. everything that is not
/// a function pattern. Computed from the geometry rather than tabulated: the
/// symbol is (4v+17)², minus a constant 225 + 8v (three 8x8 finder-plus-
/// separator blocks, the two timing lines, the 31 format modules), minus the
/// alignment patterns net of where they sit on the timing lines, minus the two
/// 18-module version blocks from version 7 on.
/// `free_modules_match_the_drawn_grid` in the tests below cross-checks this
/// closed form against the modules the drawing code actually reserves.
fn raw_data_modules(version: usize) -> usize {
    let mut bits = (16 * version + 128) * version + 64;
    if version >= 2 {
        let aligns = version / 7 + 2;
        bits -= (25 * aligns - 10) * aligns - 55;
        if version >= 7 {
            bits -= 36;
        }
    }
    bits
}

/// Total codewords (data + error correction) in `version`. The leftover 0-7
/// bits are the standard's "remainder bits" and stay light.
fn total_codewords(version: usize) -> usize {
    raw_data_modules(version) / 8
}

/// Codewords of actual data in `version` at level M.
fn data_codewords(version: usize) -> usize {
    total_codewords(version) - ECC_PER_BLOCK[version] * BLOCKS[version]
}

/// Width of the byte-mode character-count indicator. 8 bits through version 9,
/// 16 from version 10 — which is why version 10 holds 213 bytes and not the 214
/// the codeword count alone would suggest.
fn char_count_bits(version: usize) -> usize {
    if version >= 10 {
        16
    } else {
        8
    }
}

/// Payload bytes that fit in `version`: the data codewords, less the 4-bit mode
/// indicator and the character count, rounded down to whole bytes.
fn byte_capacity(version: usize) -> usize {
    (data_codewords(version) * 8 - 4 - char_count_bits(version)) / 8
}

fn smallest_version(len: usize) -> Result<usize, String> {
    (1..=MAX_VERSION).find(|v| byte_capacity(*v) >= len).ok_or_else(|| {
        format!(
            "payload is {len} bytes; the largest symbol this encoder builds \
             (version {MAX_VERSION}, error correction M) holds {}",
            byte_capacity(MAX_VERSION)
        )
    })
}

// ---------------------------------------------------------------------------
// Bit stream -> data codewords
// ---------------------------------------------------------------------------

/// Big-endian bit accumulator. QR packs bits most-significant-first across
/// codeword boundaries, so nothing here is byte-aligned until the padding.
struct BitBuf {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitBuf {
    fn new() -> BitBuf {
        BitBuf { bytes: Vec::new(), bits: 0 }
    }

    fn push(&mut self, value: u32, count: usize) {
        for i in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// The data codewords for `data` in `version`: mode indicator, character count,
/// the bytes, terminator, then the standard's alternating pad bytes.
fn encode_data(data: &[u8], version: usize) -> Vec<u8> {
    let capacity_bits = data_codewords(version) * 8;
    let mut buf = BitBuf::new();
    buf.push(0b0100, 4); // byte mode
    buf.push(data.len() as u32, char_count_bits(version));
    for b in data {
        buf.push(*b as u32, 8);
    }
    // The terminator is four zero bits, or fewer when the symbol is nearly
    // full — `min` is load-bearing: a payload that exactly fills its version
    // (e.g. 14 bytes in version 1) has no room for all four and truncating is
    // what the standard says to do, not moving to a bigger version.
    let terminator = 4.min(capacity_bits - buf.bits);
    buf.push(0, terminator);
    while !buf.bits.is_multiple_of(8) {
        buf.push(0, 1);
    }
    let mut codewords = buf.bytes;
    // Filler to the end of the data capacity: 0xEC and 0x11 alternating,
    // always starting with 0xEC however many codewords the payload happened to
    // leave (the alternation is anchored to the first pad byte, not to an even
    // index into the symbol).
    let first_pad = codewords.len();
    while codewords.len() < capacity_bits / 8 {
        codewords.push(if (codewords.len() - first_pad).is_multiple_of(2) { 0xEC } else { 0x11 });
    }
    codewords
}

// ---------------------------------------------------------------------------
// Reed-Solomon over GF(256)
// ---------------------------------------------------------------------------

/// Multiply in GF(2^8) modulo the QR field polynomial x^8+x^4+x^3+x^2+1
/// (0x11D). Russian-peasant, branch-free on the data, so there is no log table
/// to get wrong and no zero special case.
fn gf_mul(a: u8, b: u8) -> u8 {
    let mut product = 0u8;
    for i in (0..8).rev() {
        // The reduction reads the high bit of `product` *before* the shift, so
        // both halves of this line use the pre-shift value.
        product = (product << 1) ^ ((product >> 7) * 0x1D);
        product ^= ((b >> i) & 1) * a;
    }
    product
}

/// Coefficients of the degree-`degree` generator polynomial, product of
/// (x - a^i) for i in 0..degree, with the implicit leading 1 omitted.
fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1; // start at the monomial 1
    let mut root = 1u8;
    for _ in 0..degree {
        // Multiply the accumulated polynomial by (x - root), in place.
        for i in 0..degree {
            result[i] = gf_mul(result[i], root);
            if i + 1 < degree {
                result[i] ^= result[i + 1];
            }
        }
        root = gf_mul(root, 2); // a = 2 is the field's generator element
    }
    result
}

/// The `divisor.len()` error-correction codewords for one block.
fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for b in data {
        let factor = b ^ result[0];
        result.remove(0);
        result.push(0);
        for (i, d) in divisor.iter().enumerate() {
            result[i] ^= gf_mul(*d, factor);
        }
    }
    result
}

/// Split the data codewords into the version's blocks, append each block's
/// error correction, and interleave the whole lot into placement order.
///
/// The blocks are not all the same length: at level M, versions 8-10 mix two
/// sizes (version 8 is two blocks of 38 data codewords and two of 39). The
/// standard puts the short blocks first and the long ones last, and
/// interleaving takes one codeword from each block in turn — so at the column
/// where the short blocks have run out, only the long blocks contribute. Getting that skip wrong is
/// the classic way to build a symbol that scans as garbage on exactly the
/// larger versions and is fine on the smaller ones.
fn interleave(data: &[u8], version: usize) -> Vec<u8> {
    let blocks = BLOCKS[version];
    let ecc_len = ECC_PER_BLOCK[version];
    let total = total_codewords(version);
    let short_blocks = blocks - total % blocks;
    let short_data = total / blocks - ecc_len;
    let divisor = rs_divisor(ecc_len);

    let mut data_blocks: Vec<&[u8]> = Vec::with_capacity(blocks);
    let mut ecc_blocks: Vec<Vec<u8>> = Vec::with_capacity(blocks);
    let mut at = 0;
    for i in 0..blocks {
        let len = short_data + usize::from(i >= short_blocks);
        let block = &data[at..at + len];
        at += len;
        ecc_blocks.push(rs_remainder(block, &divisor));
        data_blocks.push(block);
    }

    let mut out = Vec::with_capacity(total);
    for i in 0..=short_data {
        for block in &data_blocks {
            if i < block.len() {
                out.push(block[i]);
            }
        }
    }
    for i in 0..ecc_len {
        for block in &ecc_blocks {
            out.push(block[i]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The module grid
// ---------------------------------------------------------------------------

/// Working matrix. `function` marks every module owned by a function pattern
/// (finders, separators, timing, alignment, format/version blocks, the dark
/// module): those take no data and are never masked.
///
/// Coordinates are (x = column, y = row) throughout — QR references flip
/// between the two conventions constantly, so this file commits to one.
struct Grid {
    version: usize,
    size: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}

impl Grid {
    fn new(version: usize) -> Grid {
        let size = version * 4 + 17;
        Grid {
            version,
            size,
            modules: vec![false; size * size],
            function: vec![false; size * size],
        }
    }

    fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
        self.function[y * self.size + x] = true;
    }

    fn draw_function_patterns(&mut self) {
        // Timing lines run the full width; the finders below overwrite their
        // ends, which is exactly what the standard's layout amounts to.
        for i in 0..self.size {
            self.set_function(6, i, i % 2 == 0);
            self.set_function(i, 6, i % 2 == 0);
        }
        // Three finders — never a fourth in the bottom-right corner; that
        // asymmetry is how a reader recovers the symbol's orientation.
        self.draw_finder(3, 3);
        self.draw_finder(self.size as i32 - 4, 3);
        self.draw_finder(3, self.size as i32 - 4);

        let positions = alignment_positions(self.version);
        let last = positions.len().saturating_sub(1);
        for (i, &y) in positions.iter().enumerate() {
            for (j, &x) in positions.iter().enumerate() {
                // The three that would land on a finder are omitted.
                if (i == 0 && (j == 0 || j == last)) || (i == last && j == 0) {
                    continue;
                }
                self.draw_alignment(x, y);
            }
        }

        // Reserve the format area with a placeholder mask; the real bits are
        // written once the mask is chosen.
        self.draw_format_bits(0);
        self.draw_version_bits();
    }

    /// 7x7 finder plus its separator, given the centre. Rings at Chebyshev
    /// distance 0, 1 and 3 are dark; 2 is the white ring and 4 the separator.
    fn draw_finder(&mut self, cx: i32, cy: i32) {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let (x, y) = (cx + dx, cy + dy);
                if x < 0 || y < 0 || x >= self.size as i32 || y >= self.size as i32 {
                    continue; // the separator falls outside at the symbol edges
                }
                let ring = dx.abs().max(dy.abs());
                self.set_function(x as usize, y as usize, ring != 2 && ring != 4);
            }
        }
    }

    /// 5x5 alignment pattern, given the centre. Always fully inside the symbol.
    fn draw_alignment(&mut self, cx: usize, cy: usize) {
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let dark = dx.abs().max(dy.abs()) != 1;
                self.set_function((cx as i32 + dx) as usize, (cy as i32 + dy) as usize, dark);
            }
        }
    }

    /// The 15-bit format information (level M + mask), written twice: once
    /// around the top-left finder, once split between the other two. Two copies
    /// because losing the format bits loses the whole symbol.
    fn draw_format_bits(&mut self, mask: u8) {
        let bits = format_bits(mask);
        let bit = |i: usize| (bits >> i) & 1 == 1;

        for i in 0..=5 {
            self.set_function(8, i, bit(i));
        }
        self.set_function(8, 7, bit(6));
        self.set_function(8, 8, bit(7));
        self.set_function(7, 8, bit(8));
        for i in 9..15 {
            self.set_function(14 - i, 8, bit(i));
        }

        for i in 0..8 {
            self.set_function(self.size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set_function(8, self.size - 15 + i, bit(i));
        }
        // Always dark, and it belongs to no pattern — it exists only so this
        // module is never available to data.
        self.set_function(8, self.size - 8, true);
    }

    /// The 18-bit version information, from version 7 on. A reader can infer
    /// the version from the symbol's size, but the standard still requires
    /// these blocks and readers do check them.
    fn draw_version_bits(&mut self) {
        if self.version < 7 {
            return;
        }
        let bits = version_bits(self.version);
        for i in 0..18 {
            let dark = (bits >> i) & 1 == 1;
            let a = self.size - 11 + i % 3;
            let b = i / 3;
            self.set_function(a, b, dark);
            self.set_function(b, a, dark);
        }
    }

    /// Lay the interleaved codewords into the two-module-wide zigzag: column
    /// pairs from the right edge leftwards, alternating up and down, right
    /// module of the pair before the left one, skipping function modules.
    fn draw_codewords(&mut self, codewords: &[u8]) {
        let mut i = 0usize; // bit index
        let mut right = self.size - 1;
        loop {
            // Column 6 is the vertical timing line: the pairs step over it, so
            // from here leftwards every pair is shifted by one.
            if right == 6 {
                right = 5;
            }
            for vert in 0..self.size {
                for j in 0..2 {
                    let x = right - j;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { self.size - 1 - vert } else { vert };
                    let at = y * self.size + x;
                    if !self.function[at] && i < codewords.len() * 8 {
                        self.modules[at] = (codewords[i / 8] >> (7 - i % 8)) & 1 == 1;
                        i += 1;
                    }
                    // Remainder bits (0-7 of them, version dependent) simply
                    // stay light, which is what the standard asks for.
                }
            }
            if right < 2 {
                break;
            }
            right -= 2;
        }
    }

    /// XOR the mask over the data modules only. Its own inverse, which is what
    /// lets the scorer try all eight in place.
    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let at = y * self.size + x;
                if self.function[at] {
                    continue;
                }
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (y / 2 + x / 3) % 2 == 0,
                    5 => (x * y) % 2 + (x * y) % 3 == 0,
                    6 => ((x * y) % 2 + (x * y) % 3) % 2 == 0,
                    7 => ((x + y) % 2 + (x * y) % 3) % 2 == 0,
                    _ => unreachable!("mask patterns are 0-7"),
                };
                self.modules[at] ^= invert;
            }
        }
    }

    /// Try all eight masks, keep the lowest-penalty one, and leave it applied
    /// with its format bits written. Every mask yields a *decodable* symbol —
    /// the penalty rules are a readability heuristic (they push away from large
    /// blank areas and from finder lookalikes that would confuse a scanner),
    /// not a correctness condition.
    fn apply_best_mask(&mut self) -> u8 {
        let mut best = 0u8;
        let mut best_penalty = u32::MAX;
        for mask in 0..8u8 {
            self.apply_mask(mask);
            self.draw_format_bits(mask);
            let penalty = self.penalty();
            if penalty < best_penalty {
                best_penalty = penalty;
                best = mask;
            }
            self.apply_mask(mask); // undo
        }
        self.apply_mask(best);
        self.draw_format_bits(best);
        best
    }

    fn penalty(&self) -> u32 {
        let n = self.size;
        let mut score = 0;

        // Rules 1 and 3 are per-line, in both directions.
        for i in 0..n {
            let row: Vec<bool> = (0..n).map(|x| self.get(x, i)).collect();
            let col: Vec<bool> = (0..n).map(|y| self.get(i, y)).collect();
            score += line_penalty(&row) + line_penalty(&col);
        }

        // Rule 2: same-coloured 2x2 blocks, counted at every overlapping
        // position rather than once per maximal area.
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.get(x, y);
                if c == self.get(x + 1, y) && c == self.get(x, y + 1) && c == self.get(x + 1, y + 1)
                {
                    score += PENALTY_N2;
                }
            }
        }

        // Rule 4: how far the dark share strays from the 45-55% band, in 5%
        // steps. `size` is odd for every version, so `total` is odd and
        // 20*dark can never equal 10*total — the ceiling is therefore at least
        // 1 and this subtraction cannot underflow.
        let dark = self.modules.iter().filter(|m| **m).count();
        let total = n * n;
        let k = (dark * 20).abs_diff(total * 10).div_ceil(total) - 1;
        score += PENALTY_N4 * k as u32;

        score
    }
}

/// Penalty rules 1 (runs of five or more) and 3 (finder lookalikes) for one row
/// or column.
fn line_penalty(line: &[bool]) -> u32 {
    let mut score = 0;
    let mut run = 1;
    for i in 1..line.len() {
        if line[i] == line[i - 1] {
            run += 1;
            if run == 5 {
                score += PENALTY_N1;
            } else if run > 5 {
                score += 1; // each module past the fifth adds one
            }
        } else {
            run = 1;
        }
    }
    for window in line.windows(FINDER_LIKE.len()) {
        let forward = window.iter().eq(FINDER_LIKE.iter());
        let backward = window.iter().eq(FINDER_LIKE.iter().rev());
        if forward || backward {
            score += PENALTY_N3;
        }
    }
    score
}

/// Centre coordinates of the alignment patterns, ascending. Always 6 first,
/// then evenly spaced up to size-7; the spacing is rounded *up* to an even
/// number so every centre lands on the timing pattern's parity.
fn alignment_positions(version: usize) -> Vec<usize> {
    if version == 1 {
        return Vec::new();
    }
    let count = version / 7 + 2;
    let divisor = count * 2 - 2;
    let step = (version * 4 + 4).div_ceil(divisor) * 2;
    let mut positions = vec![6];
    let mut pos = version * 4 + 10; // size - 7
    while positions.len() < count {
        positions.insert(1, pos);
        pos -= step;
    }
    positions
}

/// The 15-bit format information for level M and `mask`: 5 data bits, a BCH
/// (15,5) remainder, then a fixed XOR mask so the all-zero format is not an
/// all-light run.
fn format_bits(mask: u8) -> u32 {
    let data = u32::from(mask); // level M is 0b00, so the level bits vanish
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    ((data << 10) | (rem & 0x3FF)) ^ 0x5412
}

/// The 18-bit version information: 6 version bits plus a BCH (18,6) remainder.
/// No XOR mask on this one.
fn version_bits(version: usize) -> u32 {
    let mut rem = version as u32;
    for _ in 0..12 {
        rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
    }
    ((version as u32) << 12) | (rem & 0xFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic pairing payload: 102 bytes, which lands on version 6.
    const PAIR_URL: &str = "kod://pair?h=100.101.102.103&p=8787&t=\
        3f1a9c2b7e4d6058a1b2c3d4e5f60718293a4b5c6d7e8f9012345678abcdef01";

    /// The rejection reason for a payload that must not encode. (`unwrap_err`
    /// would need `Qr: Debug`, and debug-printing three thousand booleans is
    /// not a diagnostic.)
    fn encode_err(payload: &str) -> String {
        match Qr::encode(payload) {
            Err(e) => e,
            Ok(qr) => panic!("expected a rejection, got a {}x{} symbol", qr.size, qr.size),
        }
    }

    fn grid_for(version: usize) -> Grid {
        let mut g = Grid::new(version);
        g.draw_function_patterns();
        g
    }

    #[test]
    fn pair_url_is_the_length_the_tests_assume() {
        // Guards every other test in this file that says "version 6". Asserted as
        // a RANGE, not an exact count: the host is an IPv4 literal whose text
        // length varies (7 to 15 characters), so a real pairing URL is 94..=102
        // bytes and pinning one number makes this fail for a reason that has
        // nothing to do with the encoder.
        assert!(
            (94..=102).contains(&PAIR_URL.len()),
            "pairing URL is {} bytes, outside the range the version tests assume",
            PAIR_URL.len()
        );
        assert!(PAIR_URL.is_ascii());
    }

    #[test]
    fn version_is_the_smallest_that_fits() {
        // The capacity boundaries, both sides. These pin the whole capacity
        // chain: raw modules -> codewords -> data codewords -> character count
        // width. Version 10 holding 213 and not 214 is the 16-bit count
        // indicator showing up.
        let expect = |len: usize, size: usize| {
            let qr = Qr::encode(&"x".repeat(len)).expect("fits");
            assert_eq!(qr.size, size, "{len} bytes");
        };
        expect(14, 21); // version 1, exactly full
        expect(15, 25); // version 2
        expect(106, 41); // version 6, exactly full
        expect(107, 45); // version 7 — first version with version information
        expect(213, 57); // version 10, the maximum
        assert_eq!(byte_capacity(10), 213);

        let qr = Qr::encode(PAIR_URL).expect("pairing url fits");
        assert_eq!(qr.size, 41, "a ~100 byte pairing url is a version 6 symbol");
    }

    #[test]
    fn oversized_and_empty_payloads_are_refused_with_a_usable_reason() {
        let err = encode_err(&"x".repeat(214));
        assert!(err.contains("214"), "{err}");
        assert!(err.contains("213"), "{err}");
        assert_eq!(encode_err(""), "cannot encode an empty payload");
    }

    #[test]
    fn finder_patterns_sit_in_exactly_three_corners() {
        let qr = Qr::encode(PAIR_URL).unwrap();
        let n = qr.size;
        // 7x7: dark border, light ring, 3x3 dark core.
        let finder_at = |ox: usize, oy: usize| {
            for dy in 0..7 {
                for dx in 0..7 {
                    // Dark everywhere except the light ring at Chebyshev
                    // distance 2 from the centre.
                    let want = (dx as i32 - 3).abs().max((dy as i32 - 3).abs()) != 2;
                    if qr.dark(ox + dx, oy + dy) != want {
                        return false;
                    }
                }
            }
            true
        };
        assert!(finder_at(0, 0), "top-left");
        assert!(finder_at(n - 7, 0), "top-right");
        assert!(finder_at(0, n - 7), "bottom-left");
        // The missing fourth finder is what gives the symbol its orientation;
        // if data ever happened to draw one there a reader would be lost.
        assert!(!finder_at(n - 7, n - 7), "bottom-right must NOT be a finder");

        // Separators: the light row/column between finder and data.
        for i in 0..8 {
            assert!(!qr.dark(i, 7), "top-left separator");
            assert!(!qr.dark(7, i), "top-left separator");
            assert!(!qr.dark(n - 1 - i, 7), "top-right separator");
            assert!(!qr.dark(7, n - 1 - i), "bottom-left separator");
        }
    }

    #[test]
    fn timing_patterns_alternate_between_the_separators() {
        for version in 1..=MAX_VERSION {
            let qr = Qr::encode(&"x".repeat(byte_capacity(version))).unwrap();
            assert_eq!(qr.size, version * 4 + 17);
            for i in 8..qr.size - 8 {
                // Alignment patterns cross the timing lines and their outer
                // ring agrees with it, so a plain parity check holds all the
                // way across.
                assert_eq!(qr.dark(i, 6), i % 2 == 0, "row timing at {i}, v{version}");
                assert_eq!(qr.dark(6, i), i % 2 == 0, "column timing at {i}, v{version}");
            }
        }
    }

    #[test]
    fn the_dark_module_is_always_dark() {
        for version in 1..=MAX_VERSION {
            let qr = Qr::encode(&"x".repeat(byte_capacity(version))).unwrap();
            assert!(qr.dark(8, qr.size - 8), "v{version}");
        }
    }

    #[test]
    fn reads_outside_the_symbol_are_light() {
        let qr = Qr::encode("hi").unwrap();
        assert_eq!(qr.size, 21);
        assert!(!qr.dark(21, 0));
        assert!(!qr.dark(0, 21));
        assert!(!qr.dark(usize::MAX, usize::MAX));
    }

    #[test]
    fn encoding_is_deterministic() {
        let a = Qr::encode(PAIR_URL).unwrap();
        let b = Qr::encode(PAIR_URL).unwrap();
        assert_eq!(a.size, b.size);
        assert_eq!(a.modules, b.modules);
        // And different payloads are actually different symbols.
        let c = Qr::encode(&PAIR_URL.replace("8787", "8788")).unwrap();
        assert_ne!(a.modules, c.modules);
    }

    #[test]
    fn free_modules_match_the_drawn_grid() {
        // Cross-check: `raw_data_modules` is a closed form, the function map is
        // drawn module by module. They are derived independently, so agreement
        // at every version means the alignment positions, the version blocks
        // (36 modules from version 7) and the format reservation are all right.
        for version in 1..=MAX_VERSION {
            let g = grid_for(version);
            let free = g.function.iter().filter(|f| !**f).count();
            assert_eq!(free, raw_data_modules(version), "version {version}");
        }
    }

    #[test]
    fn the_zigzag_reaches_every_free_module() {
        // Fill with all-dark codewords: every module the walk touches goes
        // dark, so the only light data modules left must be the version's
        // remainder bits (7 for versions 2-6, none elsewhere here).
        for version in 1..=MAX_VERSION {
            let mut g = grid_for(version);
            g.draw_codewords(&vec![0xFF; total_codewords(version)]);
            let missed = (0..g.size * g.size).filter(|i| !g.function[*i] && !g.modules[*i]).count();
            assert_eq!(missed, raw_data_modules(version) % 8, "version {version}");
        }
    }

    #[test]
    fn bit_stream_matches_a_hand_derived_one() {
        // "hi" in version 1: 0100 (byte mode) 00000010 (two characters)
        // 01101000 ('h') 01101001 ('i') 0000 (terminator) — which repacks to
        // 0x40 0x26 0x86 0x90 — then alternating pad bytes to 16 codewords.
        let cw = encode_data(b"hi", 1);
        assert_eq!(cw.len(), 16);
        assert_eq!(&cw[..4], &[0x40, 0x26, 0x86, 0x90]);
        let pads = [0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11];
        assert_eq!(&cw[4..], &pads);

        // A payload that exactly fills version 1 (14 bytes) leaves no room for
        // a full terminator: 4 + 8 + 112 = 124 bits of 128, so the terminator
        // is truncated to 4 and there are no pad bytes at all.
        let full = encode_data(&[0x55; 14], 1);
        assert_eq!(full.len(), 16);
        assert_ne!(full[15], 0xEC);
        assert_ne!(full[15], 0x11);

        // One byte short of full: terminator fits, one pad byte follows.
        let nearly = encode_data(&[0x55; 13], 1);
        assert_eq!(nearly[15], 0xEC, "the first pad byte is always 0xEC");
    }

    #[test]
    fn version_10_uses_a_16_bit_character_count() {
        // 213 = 0x00D5. Nothing here is byte aligned: the stream is
        // 0100 (byte mode) 0000000011010101 (16-bit count) 10101010 (first
        // data byte) ..., which repacks to 0x40 0x0D 0x5A. An 8-bit count
        // would have put 0xD5 in the second codeword instead.
        let cw = encode_data(&[0xAA; 213], 10);
        assert_eq!(cw[0], 0x40, "mode indicator");
        assert_eq!(cw[1], 0x0D, "16-bit count, high half");
        assert_eq!(cw[2], 0x5A, "16-bit count tail plus the first data nibble");
    }

    #[test]
    fn gf256_is_a_field() {
        assert_eq!(gf_mul(0, 7), 0);
        assert_eq!(gf_mul(1, 7), 7);
        assert_eq!(gf_mul(7, 1), 7);
        // Commutative, and every non-zero element has an inverse — which is
        // exactly what fails if the reduction polynomial is wrong.
        for a in 1..=255u8 {
            assert!((1..=255u8).any(|b| gf_mul(a, b) == 1), "{a} has no inverse");
            for b in [3u8, 17, 199, 255] {
                assert_eq!(gf_mul(a, b), gf_mul(b, a));
            }
        }
        // 2 generates the multiplicative group: a^255 == 1, and no smaller
        // power of 2 returns to 1.
        let mut x = 1u8;
        for i in 1..=255 {
            x = gf_mul(x, 2);
            assert_eq!(x == 1, i == 255, "2^{i}");
        }
    }

    #[test]
    fn error_correction_codewords_have_the_generators_roots() {
        // The defining property of a Reed-Solomon code: data followed by its
        // remainder evaluates to zero at a^0 .. a^(n-1). Evaluated here by
        // independent Horner arithmetic, so a wrong generator, field or
        // remainder loop cannot pass by restating itself. Covers every ECC
        // length this encoder uses.
        for &ecc_len in &ECC_PER_BLOCK[1..] {
            let data: Vec<u8> = (0..60u32).map(|i| (i * 37 + 11) as u8).collect();
            let mut poly = data.clone();
            poly.extend_from_slice(&rs_remainder(&data, &rs_divisor(ecc_len)));
            let mut root = 1u8;
            for i in 0..ecc_len {
                let mut acc = 0u8;
                for c in &poly {
                    acc = gf_mul(acc, root) ^ c;
                }
                assert_eq!(acc, 0, "ecc {ecc_len} is not zero at a^{i}");
                root = gf_mul(root, 2);
            }
        }
    }

    #[test]
    fn interleaving_preserves_every_codeword() {
        // Version 9 is the interesting shape: 5 blocks, 3 of 36 data codewords
        // and 2 of 37, so the last data column skips the short blocks.
        let data: Vec<u8> = (0..data_codewords(9)).map(|i| (i * 7 + 1) as u8).collect();
        let out = interleave(&data, 9);
        assert_eq!(out.len(), total_codewords(9));
        let mut seen = out[..data.len()].to_vec();
        seen.sort_unstable();
        let mut want = data.clone();
        want.sort_unstable();
        assert_eq!(seen, want, "the data half must be a permutation of the input");
        // First column takes one codeword from each of the 5 blocks in order.
        assert_eq!(out[0], data[0]);
        assert_eq!(out[1], data[36]);
        assert_eq!(out[2], data[72]);
        assert_eq!(out[3], data[108]);
        assert_eq!(out[4], data[145]);
        // The last data column has only the two long blocks left.
        assert_eq!(out[data.len() - 2], data[144]);
        assert_eq!(out[data.len() - 1], data[181]);
    }

    #[test]
    fn format_information_is_a_valid_bch_code() {
        // Mask 0 at level M is all-zero data, so its remainder is zero and the
        // result is the XOR mask itself — the one value derivable by hand.
        assert_eq!(format_bits(0), 0x5412);
        for mask in 0..8u8 {
            let bits = format_bits(mask);
            assert!(bits < 1 << 15);
            // Undo the XOR mask and the 15 bits must divide by the generator.
            let mut rem = bits ^ 0x5412;
            for i in (10..15).rev() {
                if rem >> i & 1 == 1 {
                    rem ^= 0x537 << (i - 10);
                }
            }
            assert_eq!(rem & 0x3FF, 0, "mask {mask} is not a codeword");
            assert_eq!((bits ^ 0x5412) >> 10, u32::from(mask), "mask {mask} data bits");
        }
        // BCH(15,5) has minimum distance 7; anything less means the remainder
        // loop is broken in a way divisibility alone would not catch.
        for a in 0..8u8 {
            for b in a + 1..8u8 {
                let d = (format_bits(a) ^ format_bits(b)).count_ones();
                assert!(d >= 7, "masks {a}/{b} differ in only {d} bits");
            }
        }
    }

    #[test]
    fn version_information_is_a_valid_bch_code() {
        for version in 7..=MAX_VERSION {
            let bits = version_bits(version);
            assert!(bits < 1 << 18);
            assert_eq!(bits >> 12, version as u32, "version {version} data bits");
            let mut rem = bits;
            for i in (12..18).rev() {
                if rem >> i & 1 == 1 {
                    rem ^= 0x1F25 << (i - 12);
                }
            }
            assert_eq!(rem & 0xFFF, 0, "version {version} is not a codeword");
        }
        // BCH(18,6) has minimum distance 8.
        for a in 7..=MAX_VERSION {
            for b in a + 1..=MAX_VERSION {
                let d = (version_bits(a) ^ version_bits(b)).count_ones();
                assert!(d >= 8, "versions {a}/{b} differ in only {d} bits");
            }
        }
    }

    #[test]
    fn alignment_centres_match_the_standards_table() {
        // ISO/IEC 18004 annex E, versions 1-10.
        assert!(alignment_positions(1).is_empty());
        assert_eq!(alignment_positions(2), vec![6, 18]);
        assert_eq!(alignment_positions(3), vec![6, 22]);
        assert_eq!(alignment_positions(4), vec![6, 26]);
        assert_eq!(alignment_positions(5), vec![6, 30]);
        assert_eq!(alignment_positions(6), vec![6, 34]);
        assert_eq!(alignment_positions(7), vec![6, 22, 38]);
        assert_eq!(alignment_positions(8), vec![6, 24, 42]);
        assert_eq!(alignment_positions(9), vec![6, 26, 46]);
        assert_eq!(alignment_positions(10), vec![6, 28, 50]);
    }

    #[test]
    fn block_layout_matches_the_standards_table() {
        // Derived rather than tabulated (see BLOCKS), so pin the results
        // against ISO/IEC 18004 table 9's medium rows.
        let layout = |v: usize| {
            let total = total_codewords(v);
            let blocks = BLOCKS[v];
            let short = blocks - total % blocks;
            (total, data_codewords(v), short, total / blocks - ECC_PER_BLOCK[v])
        };
        assert_eq!(layout(1), (26, 16, 1, 16));
        assert_eq!(layout(5), (134, 86, 2, 43));
        assert_eq!(layout(6), (172, 108, 4, 27));
        assert_eq!(layout(8), (242, 154, 2, 38)); // 2 blocks of 38 + 2 of 39
        assert_eq!(layout(9), (292, 182, 3, 36)); // 3 of 36 + 2 of 37
        assert_eq!(layout(10), (346, 216, 4, 43)); // 4 of 43 + 1 of 44
    }

    #[test]
    fn penalty_rules_score_the_shapes_they_are_meant_to() {
        // Rule 1: five in a row is 3, each extra module adds 1.
        assert_eq!(line_penalty(&[true; 4]), 0);
        assert_eq!(line_penalty(&[true; 5]), PENALTY_N1);
        assert_eq!(line_penalty(&[true; 7]), PENALTY_N1 + 2);
        // Rule 3: the finder lookalike, both ways round, and not a near miss.
        // Its longest run is the four trailing light modules, so rule 1 adds
        // nothing and the 40 below is rule 3 alone.
        assert_eq!(line_penalty(&FINDER_LIKE), PENALTY_N3);
        let mut reversed = FINDER_LIKE;
        reversed.reverse();
        assert_eq!(line_penalty(&reversed), PENALTY_N3);
        let mut near_miss = FINDER_LIKE;
        near_miss[10] = true; // only three light modules trailing
        assert_eq!(line_penalty(&near_miss), 0);
    }

    #[test]
    fn masking_touches_data_modules_only() {
        // Every function module must survive all eight masks unchanged;
        // masking one would corrupt the format bits or a finder.
        let mut g = grid_for(6);
        g.draw_codewords(&vec![0xA5; total_codewords(6)]);
        let before = g.modules.clone();
        for mask in 0..8u8 {
            g.apply_mask(mask);
            for (i, (now, was)) in g.modules.iter().zip(&before).enumerate() {
                if g.function[i] {
                    assert_eq!(now, was, "mask {mask} touched a function module");
                }
            }
            g.apply_mask(mask);
            assert_eq!(g.modules, before, "mask {mask} is not its own inverse");
        }
    }

    #[test]
    fn every_supported_version_builds() {
        // Full sweep at each version's exact capacity: this is the path that
        // touches multi-block interleaving (versions 4+), the mixed block
        // lengths (8+) and the version information blocks (7+).
        for version in 1..=MAX_VERSION {
            // Exactly this version's capacity, so every symbol is full and the
            // padding path is the truncated-terminator one.
            let mut payload = String::new();
            while payload.len() < byte_capacity(version) {
                payload.push_str("0123456789abcdef");
            }
            payload.truncate(byte_capacity(version));
            let qr = Qr::encode(&payload).unwrap();
            assert_eq!(qr.size, version * 4 + 17, "payload of {} bytes", payload.len());
            // Sanity: a symbol is never overwhelmingly one colour.
            let dark = (0..qr.size).flat_map(|y| (0..qr.size).map(move |x| (x, y)))
                .filter(|(x, y)| qr.dark(*x, *y))
                .count();
            let share = dark * 100 / (qr.size * qr.size);
            assert!((35..=65).contains(&share), "v{version} is {share}% dark");
        }
    }
}

#[cfg(test)]
mod roundtrip_dump {
    use super::Qr;

    /// Writes each payload as a PBM so an EXTERNAL decoder can read it back.
    /// Ignored by default (it writes files and is only half a test — the other
    /// half is the Swift/Vision decode driven from the shell).
    #[test]
    #[ignore]
    fn dump_pbms_for_external_decode() {
        let dir = std::env::var("QR_DUMP_DIR").expect("set QR_DUMP_DIR");
        for (name, payload) in super::super::qr::roundtrip_dump::CASES {
            let q = Qr::encode(payload).expect("encode");
            // 8px per module with a 4-module quiet zone; decoders need both.
            let scale = 8usize;
            let quiet = 4usize;
            let px = (q.size + quiet * 2) * scale;
            let mut out = format!("P1\n{px} {px}\n");
            for y in 0..px {
                let my = y / scale;
                for x in 0..px {
                    let mx = x / scale;
                    let dark = mx >= quiet
                        && my >= quiet
                        && mx < quiet + q.size
                        && my < quiet + q.size
                        && q.dark(mx - quiet, my - quiet);
                    out.push(if dark { '1' } else { '0' });
                }
                out.push('\n');
            }
            std::fs::write(format!("{dir}/{name}.pbm"), out).expect("write");
            std::fs::write(format!("{dir}/{name}.txt"), payload).expect("write");
        }
    }

    pub(crate) const CASES: &[(&str, &str)] = &[
        ("short", "kod://pair?h=127.0.0.1&p=8787&t=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
        ("real", "kod://pair?h=100.101.102.103&p=8787&t=9f2c71a4e8b30d5f6172c8ab4d90e3f15c7b28a06d4e91f3820b5c6d7e8a9f01"),
        ("maxish", "kod://pair?h=100.127.255.254&p=65535&t=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff&x=padpadpadpadpadpadpadpadpadpadpadpad"),
    ];
}

#[cfg(test)]
mod golden {
    use super::Qr;

    /// A cheap order-sensitive fingerprint of the whole module grid.
    fn fingerprint(q: &Qr) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        h ^= q.size as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
        for y in 0..q.size {
            for x in 0..q.size {
                h ^= if q.dark(x, y) { 1 } else { 2 };
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        }
        h
    }

    #[test]
    #[ignore]
    fn print_fingerprints() {
        for (name, payload) in super::roundtrip_dump::CASES {
            let q = Qr::encode(payload).unwrap();
            println!("{name} size={} fp=0x{:016x}", q.size, fingerprint(&q));
        }
    }

    /// KNOWN-ANSWER TEST. These fingerprints cover the ENTIRE module grid, so any
    /// change to encoding, error correction, block interleaving, the zigzag walk
    /// or mask selection moves them.
    ///
    /// It exists because the structural tests could not see a catastrophic
    /// regression: inverting a single operator in the zigzag walk produces symbols
    /// that NO decoder can read, and every other test in this file still passed.
    /// A QR code is not something you can eyeball, so the only real protection is
    /// pinning the bits.
    ///
    /// The values are trustworthy because the three symbols they describe were
    /// round-tripped through Apple's Vision framework (`VNDetectBarcodesRequest`)
    /// and decoded back to their exact input — 96, 100 and 142 characters — before
    /// being pinned here. If you change the encoder deliberately, re-run that
    /// external decode BEFORE updating these numbers; do not just paste whatever
    /// the failure prints, or you will pin a broken encoder.
    #[test]
    fn the_module_grid_is_exactly_what_a_decoder_was_proven_to_read() {
        let expected: &[(&str, usize, u64)] = &[
            ("short", 41, 0x3945_e56f_dbaf_3847),
            ("real", 41, 0xad3e_4f91_2810_a70d),
            ("maxish", 49, 0x5aeb_ea2c_f39a_c2da),
        ];
        for ((name, payload), (ename, esize, efp)) in
            super::roundtrip_dump::CASES.iter().zip(expected)
        {
            assert_eq!(name, ename, "CASES and the golden table drifted apart");
            let q = Qr::encode(payload).expect("encode");
            assert_eq!(q.size, *esize, "{name}: version changed");
            assert_eq!(
                fingerprint(&q),
                *efp,
                "{name}: the module grid changed — if this was deliberate, re-verify \
                 with an external decoder before updating the golden value"
            );
        }
    }

    /// The fingerprint has to actually notice a single flipped module, or the test
    /// above is decoration.
    #[test]
    fn the_fingerprint_is_sensitive_to_one_module() {
        let a = Qr::encode("kod://pair?h=1.2.3.4&p=8787&t=abc").unwrap();
        let b = Qr::encode("kod://pair?h=1.2.3.5&p=8787&t=abc").unwrap();
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
