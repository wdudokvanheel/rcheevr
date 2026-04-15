//! Condition and condset types plus the rcheevos condition-string parser.
//!
//! An achievement string looks like:
//!   `core_cond1_cond2Salt1_cond1Salt2_cond1`
//! where `S` separates condsets and `_` separates conditions within a condset.

use crate::memaddr::{parse_operand, Operand, ParseError};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Flag prefix on a condition. The letter used in the rcheevos string is shown
/// in the variant doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionFlag {
    /// No prefix — a plain comparison condition.
    Standard,
    /// `P:` — pause hit-counting in this condset while true.
    PauseIf,
    /// `R:` — reset all hit counts in this condset while true.
    ResetIf,
    /// `Z:` — reset the hit count of the *next* condition only.
    ResetNextIf,
    /// `A:` — accumulate (add) this value into a running sum for the next row.
    AddSource,
    /// `B:` — subtract this value from the running sum for the next row.
    SubSource,
    /// `C:` — contribute hits from this condition toward the next `required_hits`.
    AddHits,
    /// `D:` — subtract hits from the running total (uncommon).
    SubHits,
    /// `N:` — AND this comparison into the next condition's gate.
    AndNext,
    /// `O:` — OR this comparison into the next condition's gate.
    OrNext,
    /// `M:` — expose hit progress as Measured value.
    Measured,
    /// `Q:` — gate whether Measured progress is reported.
    MeasuredIf,
    /// `T:` — achievement must reach this condition last ("trigger").
    Trigger,
    /// `I:` — add the value of this row to the next row's memory address.
    AddAddress,
    /// `K:` — store the LHS value into the "Remember" register.
    Remember,
}

/// Comparison or modifier operator between LHS and RHS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    /// Used on AddSource / SubSource / AddAddress modifier rows where the
    /// "operator" applies to the running accumulator rather than a comparison.
    Multiply,
    Divide,
    BitwiseAnd,
    BitwiseXor,
    Modulo,
    /// Sentinel: no operator in use (e.g. a bare AddSource row with no RHS).
    None,
}

/// A single condition: `[Flag:] lhs op rhs [.hits. | (hits)]`.
#[derive(Debug, Clone)]
pub struct Condition {
    pub flag: ConditionFlag,
    pub lhs: Operand,
    pub op: CompareOp,
    pub rhs: Operand,
    /// Number of frames the condition must be true before it is "satisfied".
    /// 0 means "must be true right now" (level-triggered).
    pub required_hits: u32,
}

/// A condset is a list of conditions that are evaluated together.
#[derive(Debug, Clone, Default)]
pub struct Condset {
    pub conditions: Vec<Condition>,
}

/// A full achievement: one core condset, zero or more alt condsets.
///
/// The achievement triggers when the core condset is satisfied **and** at least
/// one alt condset is satisfied (or there are no alts).
#[derive(Debug, Clone, Default)]
pub struct Achievement {
    pub core: Condset,
    pub alts: Vec<Condset>,
}

/// Error returned when parsing a condition string fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionParseError {
    pub pos: usize,
    pub msg: &'static str,
}

impl From<ParseError> for ConditionParseError {
    fn from(e: ParseError) -> Self {
        Self { pos: e.pos, msg: e.msg }
    }
}

impl std::fmt::Display for ConditionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "condition parse error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for ConditionParseError {}

// ---------------------------------------------------------------------------
// Achievement parser
// ---------------------------------------------------------------------------

impl Achievement {
    /// Parse a full rcheevos condition string into an `Achievement`.
    ///
    /// Format: `core_condsetScalt1_condset1Salt2_condset2...`
    /// * `S` separates condsets (core first, then alts).
    /// * `_` separates conditions within a condset.
    pub fn parse(src: &str) -> Result<Self, ConditionParseError> {
        // Split on 'S' to get raw condset strings.
        // rcheevos uses a bare 'S' as condset separator; it cannot appear inside
        // an operand (size prefix 'S' in 0xS is bit-6, but the full token is
        // "0xS..." which starts with '0', not 'S'). Safe to split naively here.
        // We need to split on 'S' that is a condset separator, not a size byte.
        // A condset separator 'S' appears at the start of the string, after '_',
        // or at the very beginning. We collect the split manually to track offsets.
        let condset_strings = split_condsets(src);

        let mut iter = condset_strings.into_iter();
        let core_str = iter.next().unwrap_or("");
        let core = parse_condset(core_str, 0)?;
        let mut alts = Vec::new();
        let mut offset = core_str.len() + 1; // +1 for 'S'
        for alt_str in iter {
            alts.push(parse_condset(alt_str, offset)?);
            offset += alt_str.len() + 1;
        }
        Ok(Achievement { core, alts })
    }
}

// ---------------------------------------------------------------------------
// Internal parser helpers
// ---------------------------------------------------------------------------

/// Split the full achievement string into condset strings on the 'S' separator.
///
/// 'S' inside a memory operand (e.g. `0xS0001`) always follows a hex-digit
/// free chain starting with `0x`, so a bare 'S' that acts as separator will
/// be preceded by a digit, letter, `)`, or `.` — never by `0x`.
/// The simplest correct rule from rcheevos: split on every bare `S` where the
/// preceding character is NOT a valid part of `0x<size>` (i.e. the S is NOT
/// immediately after `0x` with no intervening char, meaning preceded by `0x`
/// exactly). In practice rcheevos splits on *top-level* `S`: any `S` that is
/// not inside the `0x<letter>` token.
///
/// Implementation: scan left-to-right; when we see `S`, look back to confirm
/// the previous two bytes are NOT `0x` / `0X`.
fn split_condsets(src: &str) -> Vec<&str> {
    let b = src.as_bytes();
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'S' && !is_size_prefix_s(b, i) {
            result.push(&src[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    result.push(&src[start..]);
    result
}

/// Returns true if the 'S' at `pos` is part of a `0xS` / `0XS` size prefix
/// (meaning it is the Bit6 size letter, not a condset separator).
fn is_size_prefix_s(b: &[u8], pos: usize) -> bool {
    if pos >= 2 {
        matches!((b[pos - 2], b[pos - 1]), (b'0', b'x') | (b'0', b'X'))
    } else {
        false
    }
}

/// Parse a single condset string (already split on 'S') into a `Condset`.
/// `base_offset` is only used for error reporting (byte position in original src).
fn parse_condset(s: &str, base_offset: usize) -> Result<Condset, ConditionParseError> {
    if s.is_empty() {
        return Ok(Condset::default());
    }
    let mut conditions = Vec::new();
    // Split on '_'; like 'S', '_' cannot appear inside an operand token.
    for raw in s.split('_') {
        if raw.is_empty() {
            continue;
        }
        let cond = parse_condition(raw, base_offset)?;
        conditions.push(cond);
    }
    Ok(Condset { conditions })
}

/// Parse a single condition string (no '_' or 'S' separators) into a `Condition`.
///
/// Grammar: `[Flag:] lhs_operand op rhs_operand [.N. | (N)]`
///
/// Flag is a single ASCII letter followed immediately by `:`.
fn parse_condition(s: &str, _base_offset: usize) -> Result<Condition, ConditionParseError> {
    let b = s.as_bytes();
    let mut pos = 0usize;

    // ------------------------------------------------------------------
    // 1. Optional flag prefix: single letter + ':'
    // ------------------------------------------------------------------
    let flag = if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let flag_char = b[0].to_ascii_uppercase();
        pos += 2;
        parse_flag(flag_char, pos - 2)?
    } else {
        ConditionFlag::Standard
    };

    // ------------------------------------------------------------------
    // 2. LHS operand
    // ------------------------------------------------------------------
    if pos >= b.len() {
        return Err(ConditionParseError { pos, msg: "expected LHS operand" });
    }
    let (lhs, consumed) =
        parse_operand(&s[pos..]).map_err(|e| ConditionParseError { pos: pos + e.pos, msg: e.msg })?;
    pos += consumed;

    // ------------------------------------------------------------------
    // 3. Operator
    // ------------------------------------------------------------------
    if pos >= b.len() {
        // Bare operand with no op; treat as incomplete modifier row (e.g. AddSource with
        // no RHS). Default op=None, rhs=ConstInt(0).
        return Ok(Condition {
            flag,
            lhs,
            op: CompareOp::None,
            rhs: Operand::ConstInt(0),
            required_hits: 0,
        });
    }

    let (op, op_len) = parse_op(&s[pos..])?;
    pos += op_len;

    // ------------------------------------------------------------------
    // 4. RHS operand (optional for modifier-chain ops)
    // ------------------------------------------------------------------
    let rhs = if pos < b.len() {
        // Peek: if the remaining string starts with '.' or '(' it's just the hit count.
        let next = b[pos];
        if next == b'.' || next == b'(' {
            // No rhs — just hit count
            Operand::ConstInt(0)
        } else {
            let (rhs_op, rhs_consumed) = parse_operand(&s[pos..])
                .map_err(|e| ConditionParseError { pos: pos + e.pos, msg: e.msg })?;
            pos += rhs_consumed;
            rhs_op
        }
    } else {
        Operand::ConstInt(0)
    };

    // ------------------------------------------------------------------
    // 5. Optional hit count: legacy `.N.` or modern `(N)`
    // ------------------------------------------------------------------
    let required_hits = if pos < b.len() {
        parse_hit_count(s, &mut pos)?
    } else {
        0
    };

    Ok(Condition { flag, lhs, op, rhs, required_hits })
}

/// Map a flag letter (already uppercased) to a `ConditionFlag`.
fn parse_flag(c: u8, _pos: usize) -> Result<ConditionFlag, ConditionParseError> {
    match c {
        b'P' => Ok(ConditionFlag::PauseIf),
        b'R' => Ok(ConditionFlag::ResetIf),
        b'Z' => Ok(ConditionFlag::ResetNextIf),
        b'A' => Ok(ConditionFlag::AddSource),
        b'B' => Ok(ConditionFlag::SubSource),
        b'C' => Ok(ConditionFlag::AddHits),
        b'D' => Ok(ConditionFlag::SubHits),
        b'N' => Ok(ConditionFlag::AndNext),
        b'O' => Ok(ConditionFlag::OrNext),
        b'M' => Ok(ConditionFlag::Measured),
        b'Q' => Ok(ConditionFlag::MeasuredIf),
        b'T' => Ok(ConditionFlag::Trigger),
        b'I' => Ok(ConditionFlag::AddAddress),
        b'K' => Ok(ConditionFlag::Remember),
        _ => Err(ConditionParseError { pos: 0, msg: "unknown condition flag letter" }),
    }
}

/// Parse the operator string at the start of `s`.
/// Returns `(op, bytes_consumed)`.
fn parse_op(s: &str) -> Result<(CompareOp, usize), ConditionParseError> {
    let b = s.as_bytes();
    match b.get(0) {
        Some(b'=') => Ok((CompareOp::Equal, 1)),
        Some(b'!') if b.get(1) == Some(&b'=') => Ok((CompareOp::NotEqual, 2)),
        Some(b'<') if b.get(1) == Some(&b'=') => Ok((CompareOp::LessOrEqual, 2)),
        Some(b'<') => Ok((CompareOp::Less, 1)),
        Some(b'>') if b.get(1) == Some(&b'=') => Ok((CompareOp::GreaterOrEqual, 2)),
        Some(b'>') => Ok((CompareOp::Greater, 1)),
        Some(b'*') => Ok((CompareOp::Multiply, 1)),
        Some(b'/') => Ok((CompareOp::Divide, 1)),
        Some(b'&') => Ok((CompareOp::BitwiseAnd, 1)),
        Some(b'^') => Ok((CompareOp::BitwiseXor, 1)),
        Some(b'%') => Ok((CompareOp::Modulo, 1)),
        _ => Err(ConditionParseError { pos: 0, msg: "expected operator" }),
    }
}

/// Parse the trailing hit count from `s` starting at `*pos`.
/// Accepts `.N.` (legacy) and `(N)` (modern).
fn parse_hit_count(s: &str, pos: &mut usize) -> Result<u32, ConditionParseError> {
    let b = s.as_bytes();
    let start = *pos;
    match b.get(start) {
        Some(b'.') => {
            // Legacy form: `.N.`
            *pos += 1;
            let digit_start = *pos;
            while *pos < b.len() && b[*pos].is_ascii_digit() {
                *pos += 1;
            }
            if *pos == digit_start {
                return Err(ConditionParseError { pos: start, msg: "expected digits in hit count" });
            }
            let n: u32 = s[digit_start..*pos].parse().map_err(|_| ConditionParseError {
                pos: digit_start,
                msg: "hit count out of range",
            })?;
            // Consume the closing '.'
            if b.get(*pos) == Some(&b'.') {
                *pos += 1;
            }
            Ok(n)
        }
        Some(b'(') => {
            // Modern form: `(N)`
            *pos += 1;
            let digit_start = *pos;
            while *pos < b.len() && b[*pos].is_ascii_digit() {
                *pos += 1;
            }
            if *pos == digit_start {
                return Err(ConditionParseError { pos: start, msg: "expected digits in hit count" });
            }
            let n: u32 = s[digit_start..*pos].parse().map_err(|_| ConditionParseError {
                pos: digit_start,
                msg: "hit count out of range",
            })?;
            // Consume the closing ')'
            if b.get(*pos) == Some(&b')') {
                *pos += 1;
            }
            Ok(n)
        }
        _ => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memaddr::{MemSize, OperandKind};

    // -----------------------------------------------------------------------
    // Helper: assert a condition has the expected flag
    // -----------------------------------------------------------------------
    fn cond(s: &str) -> Condition {
        parse_condition(s, 0).unwrap_or_else(|e| panic!("parse_condition({:?}) failed: {:?}", s, e))
    }

    fn ach(s: &str) -> Achievement {
        Achievement::parse(s).unwrap_or_else(|e| panic!("Achievement::parse({:?}) failed: {}", s, e))
    }

    fn mem_operand(kind: OperandKind, size: MemSize, addr: u32) -> Operand {
        Operand::Memory { kind, size, address: addr }
    }

    // -----------------------------------------------------------------------
    // 1. Flag parsing
    // -----------------------------------------------------------------------

    #[test]
    fn flag_standard_no_prefix() {
        let c = cond("0xH1234=5");
        assert_eq!(c.flag, ConditionFlag::Standard);
    }

    #[test]
    fn flag_pause_if() {
        let c = cond("P:0xH1234=5");
        assert_eq!(c.flag, ConditionFlag::PauseIf);
    }

    #[test]
    fn flag_reset_if() {
        let c = cond("R:0xH1234=0");
        assert_eq!(c.flag, ConditionFlag::ResetIf);
    }

    #[test]
    fn flag_reset_next_if() {
        let c = cond("Z:0xH1234=0");
        assert_eq!(c.flag, ConditionFlag::ResetNextIf);
    }

    #[test]
    fn flag_add_source() {
        let c = cond("A:0xH0000*2");
        assert_eq!(c.flag, ConditionFlag::AddSource);
        assert_eq!(c.op, CompareOp::Multiply);
    }

    #[test]
    fn flag_sub_source() {
        let c = cond("B:0xH0000*1");
        assert_eq!(c.flag, ConditionFlag::SubSource);
    }

    #[test]
    fn flag_add_hits() {
        let c = cond("C:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::AddHits);
    }

    #[test]
    fn flag_sub_hits() {
        let c = cond("D:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::SubHits);
    }

    #[test]
    fn flag_and_next() {
        let c = cond("N:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::AndNext);
    }

    #[test]
    fn flag_or_next() {
        let c = cond("O:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::OrNext);
    }

    #[test]
    fn flag_measured() {
        let c = cond("M:0xH0000=1(10)");
        assert_eq!(c.flag, ConditionFlag::Measured);
        assert_eq!(c.required_hits, 10);
    }

    #[test]
    fn flag_measured_if() {
        let c = cond("Q:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::MeasuredIf);
    }

    #[test]
    fn flag_trigger() {
        let c = cond("T:0xH0000=1");
        assert_eq!(c.flag, ConditionFlag::Trigger);
    }

    #[test]
    fn flag_add_address() {
        let c = cond("I:0xH0000=0");
        assert_eq!(c.flag, ConditionFlag::AddAddress);
    }

    #[test]
    fn flag_remember() {
        let c = cond("K:0xH0000=0");
        assert_eq!(c.flag, ConditionFlag::Remember);
    }

    // -----------------------------------------------------------------------
    // 2. Operator parsing
    // -----------------------------------------------------------------------

    #[test]
    fn op_equal() {
        let c = cond("0xH1234=5");
        assert_eq!(c.op, CompareOp::Equal);
    }

    #[test]
    fn op_not_equal() {
        let c = cond("0xH1234!=5");
        assert_eq!(c.op, CompareOp::NotEqual);
    }

    #[test]
    fn op_less() {
        let c = cond("0xH1234<5");
        assert_eq!(c.op, CompareOp::Less);
    }

    #[test]
    fn op_less_equal() {
        let c = cond("0xH1234<=5");
        assert_eq!(c.op, CompareOp::LessOrEqual);
    }

    #[test]
    fn op_greater() {
        let c = cond("0xH1234>5");
        assert_eq!(c.op, CompareOp::Greater);
    }

    #[test]
    fn op_greater_equal() {
        let c = cond("0xH1234>=5");
        assert_eq!(c.op, CompareOp::GreaterOrEqual);
    }

    #[test]
    fn op_multiply() {
        let c = cond("A:0xH0000*3");
        assert_eq!(c.op, CompareOp::Multiply);
        assert_eq!(c.rhs, Operand::ConstInt(3));
    }

    #[test]
    fn op_divide() {
        let c = cond("A:0xH0000/4");
        assert_eq!(c.op, CompareOp::Divide);
    }

    #[test]
    fn op_bitwise_and() {
        let c = cond("A:0xH0000&255");
        assert_eq!(c.op, CompareOp::BitwiseAnd);
    }

    #[test]
    fn op_bitwise_xor() {
        let c = cond("A:0xH0000^255");
        assert_eq!(c.op, CompareOp::BitwiseXor);
    }

    #[test]
    fn op_modulo() {
        let c = cond("A:0xH0000%10");
        assert_eq!(c.op, CompareOp::Modulo);
    }

    // -----------------------------------------------------------------------
    // 3. Hit count forms
    // -----------------------------------------------------------------------

    #[test]
    fn hit_count_modern_form() {
        let c = cond("0xH1234=5(3)");
        assert_eq!(c.required_hits, 3);
    }

    #[test]
    fn hit_count_legacy_form() {
        let c = cond("0xH1234=5.3.");
        assert_eq!(c.required_hits, 3);
    }

    #[test]
    fn hit_count_zero_default() {
        let c = cond("0xH1234=5");
        assert_eq!(c.required_hits, 0);
    }

    #[test]
    fn hit_count_large() {
        let c = cond("0xH1234=5(100)");
        assert_eq!(c.required_hits, 100);
    }

    // -----------------------------------------------------------------------
    // 4. Operand types in conditions
    // -----------------------------------------------------------------------

    #[test]
    fn lhs_memory_rhs_const() {
        let c = cond("0xH1234=5");
        assert_eq!(c.lhs, mem_operand(OperandKind::Address, MemSize::Byte, 0x1234));
        assert_eq!(c.rhs, Operand::ConstInt(5));
    }

    #[test]
    fn lhs_delta_rhs_memory() {
        let c = cond("d0xH5000>0xH5000");
        assert_eq!(c.lhs, mem_operand(OperandKind::Delta, MemSize::Byte, 0x5000));
        assert_eq!(c.rhs, mem_operand(OperandKind::Address, MemSize::Byte, 0x5000));
        assert_eq!(c.op, CompareOp::Greater);
    }

    #[test]
    fn lhs_prior() {
        let c = cond("p0xH6000!=1");
        assert_eq!(c.lhs, mem_operand(OperandKind::Prior, MemSize::Byte, 0x6000));
        assert_eq!(c.op, CompareOp::NotEqual);
    }

    // -----------------------------------------------------------------------
    // 5. Multi-condset parsing
    // -----------------------------------------------------------------------

    #[test]
    fn single_condset_no_alts() {
        let a = ach("0xH1234=5_0xH1235=2");
        assert_eq!(a.core.conditions.len(), 2);
        assert!(a.alts.is_empty());
    }

    #[test]
    fn two_condsets_core_and_one_alt() {
        let a = ach("0xH1234=5S0xH5000>d0xH5000");
        assert_eq!(a.core.conditions.len(), 1);
        assert_eq!(a.alts.len(), 1);
        assert_eq!(a.alts[0].conditions.len(), 1);
    }

    #[test]
    fn three_condsets_core_and_two_alts() {
        // From the spec example
        let a = ach("0xH1234=5_0xH1235=2S0xH5000>d0xH5000S0xH6000!=1");
        assert_eq!(a.core.conditions.len(), 2);
        assert_eq!(a.alts.len(), 2);
        assert_eq!(a.alts[0].conditions.len(), 1);
        assert_eq!(a.alts[1].conditions.len(), 1);
    }

    #[test]
    fn condset_with_flags() {
        let a = ach("R:0xH1234=5_P:0xH1235=1");
        assert_eq!(a.core.conditions[0].flag, ConditionFlag::ResetIf);
        assert_eq!(a.core.conditions[1].flag, ConditionFlag::PauseIf);
    }

    // -----------------------------------------------------------------------
    // 6. AddSource bare row (no rhs)
    // -----------------------------------------------------------------------

    #[test]
    fn add_source_no_rhs_defaults() {
        // `A:0xH1234` with no op — should not error
        let c = cond("A:0xH1234");
        assert_eq!(c.flag, ConditionFlag::AddSource);
        assert_eq!(c.op, CompareOp::None);
        assert_eq!(c.rhs, Operand::ConstInt(0));
    }

    // -----------------------------------------------------------------------
    // 7. Parser error cases
    // -----------------------------------------------------------------------

    #[test]
    fn error_unknown_flag() {
        let r = Achievement::parse("X:0xH1234=5");
        assert!(r.is_err());
    }

    #[test]
    fn error_bad_operand() {
        let r = Achievement::parse("@bad=5");
        assert!(r.is_err());
    }

    #[test]
    fn error_missing_operator() {
        // bare operand in a non-modifier context — we accept it as "bare row"
        // so no error is expected
        let c = cond("0xH1234");
        assert_eq!(c.op, CompareOp::None);
    }

    // -----------------------------------------------------------------------
    // 8. Bit-6 size prefix 'S' must NOT be treated as condset separator
    // -----------------------------------------------------------------------

    #[test]
    fn bit6_size_not_condset_split() {
        // 0xS0001 uses bit-6 size; the 'S' after '0x' must not split
        let a = ach("0xS0001=1");
        assert_eq!(a.core.conditions.len(), 1);
        assert!(a.alts.is_empty());
        // Check that the lhs is a Bit6 memory ref
        match &a.core.conditions[0].lhs {
            Operand::Memory { size: crate::memaddr::MemSize::Bit6, .. } => {}
            other => panic!("expected Bit6 memory, got {:?}", other),
        }
    }
}
