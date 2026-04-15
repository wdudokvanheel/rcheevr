//! rcheevos memaddr expression parser.
//!
//! Grammar covers size prefixes (`0xH`, `0x ` (space = 16-bit), `0xW`, `0xX`,
//! `0xL`, `0xU`, `0xM`..`0xT`, bitcount, BCD, inverted, float variants) and
//! modifiers (`d` delta, `p` prior, `b` BCD, `~` inverted).

use std::fmt;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The width / interpretation of a memory read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemSize {
    Bit0,
    Bit1,
    Bit2,
    Bit3,
    Bit4,
    Bit5,
    Bit6,
    Bit7,
    LowNibble,
    HighNibble,
    Byte,
    Word,               // 16-bit little-endian
    ThreeBytes,         // 24-bit little-endian
    DWord,              // 32-bit little-endian
    BigEndianWord,      // 16-bit big-endian
    BigEndianThreeBytes, // 24-bit big-endian
    BigEndianDWord,     // 32-bit big-endian
    BitCount,
    Float,              // 32-bit IEEE 754 little-endian
    FloatBigEndian,     // 32-bit IEEE 754 big-endian
    MbfFloat,           // 32-bit Microsoft Binary Format little-endian
    MbfFloatLittleEndian, // alias kept for symmetry with rcheevos naming
    Double32,           // 32-bit double (little-endian)
    Double32BigEndian,  // 32-bit double (big-endian)
}

/// Which snapshot of memory this operand reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    /// Current frame value.
    Address,
    /// Previous frame's value (delta).
    Delta,
    /// Value from before the most recent change.
    Prior,
    /// BCD-decoded current value.
    Bcd,
    /// Bitwise-inverted current value.
    Inverted,
    /// Reserved: recall (future stacking support).
    Recall,
}

/// A parsed operand from a rcheevos condition string.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// A memory reference: a sized read at `address`, with a snapshot modifier.
    Memory {
        kind: OperandKind,
        size: MemSize,
        address: u32,
    },
    /// An integer constant.
    ConstInt(u32),
    /// A floating-point constant.
    ConstFloat(f32),
}

/// Error returned when parsing fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset within the input string where the error was detected.
    pub pos: usize,
    /// Short description of the problem.
    pub msg: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse one operand from the beginning of `s`.
///
/// Returns `(operand, bytes_consumed)` on success.  The caller can slice
/// `&s[bytes_consumed..]` to continue parsing the rest of an expression.
///
/// # Errors
/// Returns a [`ParseError`] with `pos` pointing at the first unrecognised or
/// unexpected byte.
pub fn parse_operand(s: &str) -> Result<(Operand, usize), ParseError> {
    let b = s.as_bytes();
    if b.is_empty() {
        return Err(ParseError { pos: 0, msg: "empty input" });
    }

    let mut pos = 0;

    // ------------------------------------------------------------------
    // Step 1: consume an optional modifier prefix.
    // Recognised prefixes: d (delta), p (prior), b (bcd), ~ (inverted).
    // The `b` prefix is tricky: it must be followed by `0x` or `d`/`p`/`~`
    // to distinguish it from a bare `b` hex digit in an immediate.  We use a
    // look-ahead to decide.
    // ------------------------------------------------------------------

    let kind = match b.get(pos).copied() {
        Some(b'd') if looks_like_mem_start(b, pos + 1) => {
            // `d` followed by a memory operand start → delta modifier
            pos += 1;
            OperandKind::Delta
        }
        Some(b'p') if looks_like_mem_start(b, pos + 1) => {
            pos += 1;
            OperandKind::Prior
        }
        Some(b'b') if looks_like_mem_start(b, pos + 1) => {
            pos += 1;
            OperandKind::Bcd
        }
        Some(b'~') => {
            pos += 1;
            OperandKind::Inverted
        }
        _ => OperandKind::Address,
    };

    // ------------------------------------------------------------------
    // Step 2: what follows the (optional) modifier?
    // ------------------------------------------------------------------

    // Case A: `0x...` or `0X...` memory reference
    if b.get(pos).copied() == Some(b'0')
        && matches!(b.get(pos + 1).copied(), Some(b'x') | Some(b'X'))
    {
        pos += 2; // skip "0x"

        // The next character is the size-type letter (or a space for 16-bit).
        let size_byte = b.get(pos).copied().ok_or(ParseError {
            pos,
            msg: "expected size prefix after 0x",
        })?;

        let size = match size_byte {
            b' ' => { pos += 1; MemSize::Word }                  // 0x  → 16-bit LE
            b'H' | b'h' => { pos += 1; MemSize::Byte }           // 8-bit
            b'X' | b'x' => { pos += 1; MemSize::DWord }          // 32-bit LE
            b'W' | b'w' => { pos += 1; MemSize::ThreeBytes }     // 24-bit LE
            b'L' | b'l' => { pos += 1; MemSize::LowNibble }      // bits 0-3
            b'U' | b'u' => { pos += 1; MemSize::HighNibble }     // bits 4-7
            b'M' | b'm' => { pos += 1; MemSize::Bit0 }
            b'N' | b'n' => { pos += 1; MemSize::Bit1 }
            b'O' | b'o' => { pos += 1; MemSize::Bit2 }
            b'P' | b'p' => { pos += 1; MemSize::Bit3 }
            b'Q' | b'q' => { pos += 1; MemSize::Bit4 }
            b'R' | b'r' => { pos += 1; MemSize::Bit5 }
            b'S' | b's' => { pos += 1; MemSize::Bit6 }
            b'T' | b't' => { pos += 1; MemSize::Bit7 }
            b'K' | b'k' => { pos += 1; MemSize::BitCount }       // popcount
            b'I' | b'i' => { pos += 1; MemSize::BigEndianWord }  // 16-bit BE
            b'J' | b'j' => { pos += 1; MemSize::BigEndianThreeBytes } // 24-bit BE
            b'G' | b'g' => { pos += 1; MemSize::BigEndianDWord } // 32-bit BE
            _ => {
                return Err(ParseError { pos, msg: "unknown size prefix letter after 0x" });
            }
        };

        // Now parse the hex address (1..=8 hex digits).
        let addr_start = pos;
        while pos < b.len() && is_hex_digit(b[pos]) {
            pos += 1;
        }
        if pos == addr_start {
            return Err(ParseError { pos, msg: "expected hex address digits" });
        }
        let hex_len = pos - addr_start;
        if hex_len > 8 {
            return Err(ParseError { pos: addr_start, msg: "address too long (max 8 hex digits)" });
        }
        let addr_str = &s[addr_start..pos];
        let address = u32::from_str_radix(addr_str, 16)
            .map_err(|_| ParseError { pos: addr_start, msg: "invalid hex address" })?;

        return Ok((Operand::Memory { kind, size, address }, pos));
    }

    // Case B: `f` prefix — float-type memory reference or float immediate
    if b.get(pos).copied() == Some(b'f') || b.get(pos).copied() == Some(b'F') {
        pos += 1; // skip 'f'

        let next = b.get(pos).copied().ok_or(ParseError {
            pos,
            msg: "expected size letter or digit after 'f'",
        })?;

        match next {
            // Float memory-reference size prefixes (rcheevos uses lowercase)
            b'F' | b'f' => {
                // fF = 32-bit float LE  (or double variant — rcheevos 'fF' is float, 'dF' is double)
                // Here `f` followed by `F`: 32-bit IEEE float LE
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::Float, address }, pos));
            }
            b'B' | b'b' => {
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::FloatBigEndian, address }, pos));
            }
            b'H' | b'h' => {
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::MbfFloat, address }, pos));
            }
            b'M' | b'm' => {
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::MbfFloatLittleEndian, address }, pos));
            }
            b'I' | b'i' => {
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::Double32, address }, pos));
            }
            b'L' | b'l' => {
                pos += 1;
                let address = parse_hex_address(s, b, &mut pos)?;
                return Ok((Operand::Memory { kind, size: MemSize::Double32BigEndian, address }, pos));
            }
            // Digits or '-' / '+' / '.' → float immediate literal: f1.5, f-3.0
            b'0'..=b'9' | b'-' | b'+' | b'.' => {
                // Parse a float literal from here.
                let lit_start = pos;
                // Consume sign
                if matches!(b.get(pos).copied(), Some(b'-') | Some(b'+')) {
                    pos += 1;
                }
                // Integer part
                while pos < b.len() && b[pos].is_ascii_digit() {
                    pos += 1;
                }
                // Fractional part
                if pos < b.len() && b[pos] == b'.' {
                    pos += 1;
                    while pos < b.len() && b[pos].is_ascii_digit() {
                        pos += 1;
                    }
                }
                if pos == lit_start {
                    return Err(ParseError { pos: lit_start, msg: "expected float literal digits" });
                }
                let float_str = &s[lit_start..pos];
                let value: f32 = float_str.parse().map_err(|_| ParseError {
                    pos: lit_start,
                    msg: "malformed float literal",
                })?;
                return Ok((Operand::ConstFloat(value), pos));
            }
            _ => {
                return Err(ParseError { pos, msg: "unknown float size prefix" });
            }
        }
    }

    // Case C: `h` or `H` prefix → hex immediate (e.g. `h1f`)
    if matches!(b.get(pos).copied(), Some(b'h') | Some(b'H')) {
        pos += 1;
        let hex_start = pos;
        while pos < b.len() && is_hex_digit(b[pos]) {
            pos += 1;
        }
        if pos == hex_start {
            return Err(ParseError { pos, msg: "expected hex digits after 'h'" });
        }
        let value = u32::from_str_radix(&s[hex_start..pos], 16)
            .map_err(|_| ParseError { pos: hex_start, msg: "invalid hex immediate" })?;
        return Ok((Operand::ConstInt(value), pos));
    }

    // Case D: `v` or `V` prefix → explicit decimal immediate (e.g. `v1234`)
    if matches!(b.get(pos).copied(), Some(b'v') | Some(b'V')) {
        pos += 1;
        let dec_start = pos;
        // Allow optional leading sign
        if matches!(b.get(pos).copied(), Some(b'-') | Some(b'+')) {
            pos += 1;
        }
        let digit_start = pos;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == digit_start {
            return Err(ParseError { pos, msg: "expected decimal digits after 'v'" });
        }
        let dec_str = &s[dec_start..pos];
        // rcheevos treats the value as unsigned; a negative prefix wraps around in u32
        let value: u32 = if dec_str.starts_with('-') {
            let mag: u32 = dec_str[1..].parse().map_err(|_| ParseError {
                pos: dec_start,
                msg: "invalid decimal immediate",
            })?;
            0u32.wrapping_sub(mag)
        } else {
            dec_str.parse().map_err(|_| ParseError {
                pos: dec_start,
                msg: "invalid decimal immediate",
            })?
        };
        return Ok((Operand::ConstInt(value), pos));
    }

    // Case E: bare decimal immediate (e.g. `1234`)
    if b.get(pos).map_or(false, |c| c.is_ascii_digit()) {
        let dec_start = pos;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
        let value: u32 = s[dec_start..pos].parse().map_err(|_| ParseError {
            pos: dec_start,
            msg: "decimal immediate out of range",
        })?;
        return Ok((Operand::ConstInt(value), pos));
    }

    Err(ParseError { pos, msg: "unrecognised operand" })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns true if the byte is a valid ASCII hex digit.
#[inline]
fn is_hex_digit(c: u8) -> bool {
    c.is_ascii_digit() || matches!(c, b'a'..=b'f' | b'A'..=b'F')
}

/// Returns true if the bytes at `offset` look like the start of a memory
/// operand (`0x`, `0X`, or `f` / `F` followed by a size letter).
/// Used for modifier prefix look-ahead to avoid consuming `b` or `p` in hex
/// immediates.
fn looks_like_mem_start(b: &[u8], offset: usize) -> bool {
    match b.get(offset).copied() {
        Some(b'0') => matches!(b.get(offset + 1).copied(), Some(b'x') | Some(b'X')),
        Some(b'f') | Some(b'F') => {
            // Could be fF, fB, fH, fM, fI, fL
            matches!(
                b.get(offset + 1).copied(),
                Some(b'F' | b'f' | b'B' | b'b' | b'H' | b'h' | b'M' | b'm' | b'I' | b'i' | b'L' | b'l')
            )
        }
        // Also allow stacked modifier prefixes: b0x..., bd0x..., etc.
        Some(b'd') | Some(b'p') | Some(b'b') | Some(b'~') => true,
        _ => false,
    }
}

/// Parse a hex address from the current position, advancing `pos`.
fn parse_hex_address(s: &str, b: &[u8], pos: &mut usize) -> Result<u32, ParseError> {
    let start = *pos;
    while *pos < b.len() && is_hex_digit(b[*pos]) {
        *pos += 1;
    }
    if *pos == start {
        return Err(ParseError { pos: start, msg: "expected hex address digits" });
    }
    if *pos - start > 8 {
        return Err(ParseError { pos: start, msg: "address too long (max 8 hex digits)" });
    }
    u32::from_str_radix(&s[start..*pos], 16)
        .map_err(|_| ParseError { pos: start, msg: "invalid hex address" })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Convenience: parse and assert success.
    fn ok(s: &str) -> (Operand, usize) {
        parse_operand(s).unwrap_or_else(|e| panic!("expected Ok for {:?}, got {:?}", s, e))
    }

    // Convenience: parse and assert error.
    fn err(s: &str) -> ParseError {
        parse_operand(s).unwrap_err()
    }

    // Convenience: assert memory operand fields.
    fn mem(s: &str) -> (OperandKind, MemSize, u32, usize) {
        let (op, consumed) = ok(s);
        match op {
            Operand::Memory { kind, size, address } => (kind, size, address, consumed),
            other => panic!("expected Memory for {:?}, got {:?}", s, other),
        }
    }

    // -----------------------------------------------------------------------
    // 1. Size prefixes
    // -----------------------------------------------------------------------

    #[test]
    fn size_byte_upper_h() {
        let (_, size, addr, _) = mem("0xH1234");
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn size_byte_lower_h() {
        let (_, size, addr, _) = mem("0xh00ff");
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x00ff);
    }

    #[test]
    fn size_word_space() {
        // "0x 1234" — space is the 16-bit size prefix
        let (_, size, addr, _) = mem("0x 1234");
        assert_eq!(size, MemSize::Word);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn size_dword_x() {
        let (_, size, addr, _) = mem("0xX00001234");
        assert_eq!(size, MemSize::DWord);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn size_three_bytes_w() {
        let (_, size, addr, _) = mem("0xWabcd");
        assert_eq!(size, MemSize::ThreeBytes);
        assert_eq!(addr, 0xabcd);
    }

    #[test]
    fn size_low_nibble_l() {
        let (_, size, addr, _) = mem("0xL0010");
        assert_eq!(size, MemSize::LowNibble);
        assert_eq!(addr, 0x0010);
    }

    #[test]
    fn size_high_nibble_u() {
        let (_, size, addr, _) = mem("0xU0020");
        assert_eq!(size, MemSize::HighNibble);
        assert_eq!(addr, 0x0020);
    }

    #[test]
    fn size_bits_m_through_t() {
        let cases: &[(&str, MemSize)] = &[
            ("0xM0001", MemSize::Bit0),
            ("0xN0001", MemSize::Bit1),
            ("0xO0001", MemSize::Bit2),
            ("0xP0001", MemSize::Bit3),
            ("0xQ0001", MemSize::Bit4),
            ("0xR0001", MemSize::Bit5),
            ("0xS0001", MemSize::Bit6),
            ("0xT0001", MemSize::Bit7),
        ];
        for (s, expected) in cases {
            let (_, size, _, _) = mem(s);
            assert_eq!(size, *expected, "for input {:?}", s);
        }
    }

    #[test]
    fn size_bitcount_k() {
        let (_, size, addr, _) = mem("0xK0042");
        assert_eq!(size, MemSize::BitCount);
        assert_eq!(addr, 0x0042);
    }

    #[test]
    fn size_be_word_i() {
        let (_, size, addr, _) = mem("0xI0100");
        assert_eq!(size, MemSize::BigEndianWord);
        assert_eq!(addr, 0x0100);
    }

    #[test]
    fn size_be_three_bytes_j() {
        let (_, size, addr, _) = mem("0xJ0200");
        assert_eq!(size, MemSize::BigEndianThreeBytes);
        assert_eq!(addr, 0x0200);
    }

    #[test]
    fn size_be_dword_g() {
        let (_, size, addr, _) = mem("0xG0300");
        assert_eq!(size, MemSize::BigEndianDWord);
        assert_eq!(addr, 0x0300);
    }

    // -----------------------------------------------------------------------
    // 2. Float memory size prefixes
    // -----------------------------------------------------------------------

    #[test]
    fn float_size_f_le() {
        let (_, size, addr, _) = mem("fF1234");
        assert_eq!(size, MemSize::Float);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn float_size_f_be() {
        let (_, size, addr, _) = mem("fB1234");
        assert_eq!(size, MemSize::FloatBigEndian);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn float_size_mbf() {
        let (_, size, addr, _) = mem("fH1234");
        assert_eq!(size, MemSize::MbfFloat);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn float_size_mbf_le() {
        let (_, size, addr, _) = mem("fM1234");
        assert_eq!(size, MemSize::MbfFloatLittleEndian);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn float_size_double32() {
        let (_, size, addr, _) = mem("fI1234");
        assert_eq!(size, MemSize::Double32);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn float_size_double32_be() {
        let (_, size, addr, _) = mem("fL1234");
        assert_eq!(size, MemSize::Double32BigEndian);
        assert_eq!(addr, 0x1234);
    }

    // -----------------------------------------------------------------------
    // 3. Modifier prefixes
    // -----------------------------------------------------------------------

    #[test]
    fn modifier_delta_d() {
        let (kind, size, addr, _) = mem("d0xH1234");
        assert_eq!(kind, OperandKind::Delta);
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x1234);
    }

    #[test]
    fn modifier_prior_p() {
        let (kind, size, addr, _) = mem("p0xH5678");
        assert_eq!(kind, OperandKind::Prior);
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x5678);
    }

    #[test]
    fn modifier_bcd_b() {
        let (kind, size, addr, _) = mem("b0xH0001");
        assert_eq!(kind, OperandKind::Bcd);
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x0001);
    }

    #[test]
    fn modifier_inverted_tilde() {
        let (kind, size, addr, _) = mem("~0xH0001");
        assert_eq!(kind, OperandKind::Inverted);
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0x0001);
    }

    #[test]
    fn no_modifier_is_address() {
        let (kind, _, _, _) = mem("0xH0001");
        assert_eq!(kind, OperandKind::Address);
    }

    #[test]
    fn modifier_delta_dword() {
        let (kind, size, addr, _) = mem("d0xX0000c000");
        assert_eq!(kind, OperandKind::Delta);
        assert_eq!(size, MemSize::DWord);
        assert_eq!(addr, 0xc000);
    }

    #[test]
    fn modifier_bcd_word() {
        let (kind, size, addr, _) = mem("b0x 1000");
        assert_eq!(kind, OperandKind::Bcd);
        assert_eq!(size, MemSize::Word);
        assert_eq!(addr, 0x1000);
    }

    // -----------------------------------------------------------------------
    // 4. Address parsing edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn address_single_digit() {
        let (_, _, addr, consumed) = mem("0xH1");
        assert_eq!(addr, 1);
        assert_eq!(consumed, 4);
    }

    #[test]
    fn address_max_8_digits() {
        // "0xH" = 3 bytes + 8 hex digits = 11 bytes total
        let (_, _, addr, consumed) = mem("0xHffffffff");
        assert_eq!(addr, 0xffffffff);
        assert_eq!(consumed, 11);
    }

    #[test]
    fn address_mixed_case_hex() {
        let (_, _, addr, _) = mem("0xHAbCdEf");
        assert_eq!(addr, 0xabcdef);
    }

    #[test]
    fn consumed_length_stops_at_non_hex() {
        // "0xH1234+more" — parser should consume exactly 7 bytes
        let (_, _, addr, consumed) = mem("0xH1234+more");
        assert_eq!(addr, 0x1234);
        assert_eq!(consumed, 7);
    }

    #[test]
    fn consumed_length_for_word() {
        // "0x 0001" is 7 bytes
        let (_, _, addr, consumed) = mem("0x 0001");
        assert_eq!(addr, 1);
        assert_eq!(consumed, 7);
    }

    // -----------------------------------------------------------------------
    // 5. Immediate values
    // -----------------------------------------------------------------------

    #[test]
    fn immediate_hex_lowercase_h() {
        // "h" + "1f" = 3 bytes
        let (op, consumed) = ok("h1f");
        assert_eq!(op, Operand::ConstInt(0x1f));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn immediate_hex_uppercase_h() {
        let (op, _) = ok("H1F");
        assert_eq!(op, Operand::ConstInt(0x1f));
    }

    #[test]
    fn immediate_decimal_bare() {
        let (op, consumed) = ok("1234");
        assert_eq!(op, Operand::ConstInt(1234));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn immediate_decimal_zero() {
        let (op, consumed) = ok("0");
        assert_eq!(op, Operand::ConstInt(0));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn immediate_decimal_v_prefix() {
        let (op, consumed) = ok("v100");
        assert_eq!(op, Operand::ConstInt(100));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn immediate_decimal_v_uppercase() {
        let (op, _) = ok("V42");
        assert_eq!(op, Operand::ConstInt(42));
    }

    #[test]
    fn immediate_float_positive() {
        let (op, consumed) = ok("f1.5");
        assert_eq!(op, Operand::ConstFloat(1.5));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn immediate_float_zero() {
        let (op, _) = ok("f0.0");
        assert_eq!(op, Operand::ConstFloat(0.0));
    }

    #[test]
    fn immediate_float_integer_part_only() {
        let (op, consumed) = ok("f3");
        assert_eq!(op, Operand::ConstFloat(3.0));
        assert_eq!(consumed, 2);
    }

    #[test]
    fn immediate_float_negative() {
        let (op, _) = ok("f-2.5");
        assert_eq!(op, Operand::ConstFloat(-2.5));
    }

    #[test]
    fn immediate_hex_large() {
        let (op, _) = ok("hFF");
        assert_eq!(op, Operand::ConstInt(0xFF));
    }

    // -----------------------------------------------------------------------
    // 6. Consumed-length correctness for chaining
    // -----------------------------------------------------------------------

    #[test]
    fn chaining_two_operands() {
        // Simulates "lhs_op_rhs" where both sides are operands.
        let s = "0xH1234>0xH5678";
        let (op1, c1) = ok(s);
        assert_eq!(op1, Operand::Memory { kind: OperandKind::Address, size: MemSize::Byte, address: 0x1234 });
        assert_eq!(c1, 7);
        // Skip the '>' operator character
        let (op2, c2) = ok(&s[c1 + 1..]);
        assert_eq!(op2, Operand::Memory { kind: OperandKind::Address, size: MemSize::Byte, address: 0x5678 });
        assert_eq!(c2, 7);
    }

    #[test]
    fn chaining_immediate_then_mem() {
        let s = "1234=0xH0010";
        let (op1, c1) = ok(s);
        assert_eq!(op1, Operand::ConstInt(1234));
        assert_eq!(c1, 4);
        let (op2, _) = ok(&s[c1 + 1..]);
        assert_eq!(op2, Operand::Memory { kind: OperandKind::Address, size: MemSize::Byte, address: 0x0010 });
    }

    // -----------------------------------------------------------------------
    // 7. Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn error_empty_string() {
        let e = err("");
        assert_eq!(e.pos, 0);
    }

    #[test]
    fn error_unknown_size_prefix() {
        // 0xZ is not a valid size letter
        let e = err("0xZ1234");
        assert_eq!(e.pos, 2); // points at 'Z'
    }

    #[test]
    fn error_missing_address_after_prefix() {
        let e = err("0xH");
        assert_eq!(e.pos, 3);
    }

    #[test]
    fn error_missing_address_after_space_prefix() {
        // "0x " with no hex digits after the space
        let e = err("0x ");
        assert_eq!(e.pos, 3);
    }

    #[test]
    fn error_h_with_no_digits() {
        let e = err("h");
        assert!(e.msg.contains("hex digits") || !e.msg.is_empty());
    }

    #[test]
    fn error_v_with_no_digits() {
        let e = err("v");
        assert!(!e.msg.is_empty());
    }

    #[test]
    fn error_0x_truncated() {
        let e = err("0x");
        assert_eq!(e.pos, 2);
    }

    #[test]
    fn error_tilde_alone() {
        // '~' without a following operand — error reported after the '~', at pos 1
        let e = err("~");
        assert!(e.pos <= 1, "expected error pos <= 1, got {}", e.pos);
    }

    #[test]
    fn error_completely_unknown() {
        let e = err("@1234");
        assert_eq!(e.pos, 0);
    }

    // -----------------------------------------------------------------------
    // 8. Display impl sanity check
    // -----------------------------------------------------------------------

    #[test]
    fn parse_error_display() {
        let e = ParseError { pos: 3, msg: "test error" };
        let s = format!("{}", e);
        assert!(s.contains("3"));
        assert!(s.contains("test error"));
    }

    // -----------------------------------------------------------------------
    // 9. Lowercase 0x prefix
    // -----------------------------------------------------------------------

    #[test]
    fn prefix_0x_lowercase() {
        let (_, size, addr, _) = mem("0xH0000");
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 0);
    }

    #[test]
    fn prefix_0_capital_x() {
        // rcheevos accepts 0X as well as 0x
        let (_, size, addr, _) = mem("0XH0001");
        assert_eq!(size, MemSize::Byte);
        assert_eq!(addr, 1);
    }

    // -----------------------------------------------------------------------
    // 10. Float memory operands with modifiers
    // -----------------------------------------------------------------------

    #[test]
    fn float_mem_with_delta() {
        // "dfF1234" — delta of a 32-bit float at 0x1234
        let (kind, size, addr, consumed) = mem("dfF1234");
        assert_eq!(kind, OperandKind::Delta);
        assert_eq!(size, MemSize::Float);
        assert_eq!(addr, 0x1234);
        assert_eq!(consumed, 7);
    }

    // -----------------------------------------------------------------------
    // 11. Bit sizes with delta modifier
    // -----------------------------------------------------------------------

    #[test]
    fn bit0_with_delta() {
        let (kind, size, addr, _) = mem("d0xM0001");
        assert_eq!(kind, OperandKind::Delta);
        assert_eq!(size, MemSize::Bit0);
        assert_eq!(addr, 1);
    }

    #[test]
    fn bit7_no_modifier() {
        let (kind, size, addr, _) = mem("0xT00fe");
        assert_eq!(kind, OperandKind::Address);
        assert_eq!(size, MemSize::Bit7);
        assert_eq!(addr, 0xfe);
    }
}
