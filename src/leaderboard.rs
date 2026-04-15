//! Leaderboard condset wiring.
//!
//! Each leaderboard = START, CANCEL, SUBMIT, VALUE condsets. Reuses the
//! evaluator and emits state-transition events.

use crate::condition::{Achievement, CompareOp, ConditionFlag, ConditionParseError, Condset};
use crate::evaluator::{AchievementState, ConditionState, CondsetState, eval_achievement};
use crate::memaddr::{MemSize, Operand, OperandKind};

// ---------------------------------------------------------------------------
// ValueFormat
// ---------------------------------------------------------------------------

/// How the submitted integer score should be displayed to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFormat {
    Value,
    Score,
    Frames,
    TimeSecs,
    Minutes,
    Millisecs,
    Fixed1,
    Fixed2,
    Fixed3,
    Unsigned,
    Other,
}

impl ValueFormat {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "VALUE" => ValueFormat::Value,
            "SCORE" => ValueFormat::Score,
            "FRAMES" | "TIME" | "TIMESECS" | "SECS" => ValueFormat::TimeSecs,
            "MINUTES" => ValueFormat::Minutes,
            "MILLISECS" => ValueFormat::Millisecs,
            "FIXED1" => ValueFormat::Fixed1,
            "FIXED2" => ValueFormat::Fixed2,
            "FIXED3" => ValueFormat::Fixed3,
            "UNSIGNED" => ValueFormat::Unsigned,
            _ => ValueFormat::Other,
        }
    }

    pub fn format(self, v: i64) -> String {
        match self {
            ValueFormat::Value | ValueFormat::Unsigned | ValueFormat::Other => v.to_string(),
            ValueFormat::Score => format!("{:06}", v),
            ValueFormat::TimeSecs => {
                let total = v.unsigned_abs();
                let h = total / 3600;
                let m = (total / 60) % 60;
                let s = total % 60;
                if h > 0 {
                    format!("{}h{:02}:{:02}", h, m, s)
                } else {
                    format!("{}:{:02}", m, s)
                }
            }
            ValueFormat::Frames => {
                let secs = v.unsigned_abs() / 60;
                let h = secs / 3600;
                let m = (secs / 60) % 60;
                let s = secs % 60;
                if h > 0 {
                    format!("{}h{:02}:{:02}", h, m, s)
                } else {
                    format!("{}:{:02}", m, s)
                }
            }
            ValueFormat::Minutes => {
                let total = v.unsigned_abs();
                let h = total / 60;
                let m = total % 60;
                format!("{}h{:02}", h, m)
            }
            ValueFormat::Millisecs => {
                let total = v.unsigned_abs();
                let s = total / 100;
                let cs = total % 100;
                format!("{}.{:02}", s, cs)
            }
            ValueFormat::Fixed1 => {
                let abs = v.unsigned_abs();
                format!("{}{}.{}", if v < 0 { "-" } else { "" }, abs / 10, abs % 10)
            }
            ValueFormat::Fixed2 => {
                let abs = v.unsigned_abs();
                format!("{}{}.{:02}", if v < 0 { "-" } else { "" }, abs / 100, abs % 100)
            }
            ValueFormat::Fixed3 => {
                let abs = v.unsigned_abs();
                format!("{}{}.{:03}", if v < 0 { "-" } else { "" }, abs / 1000, abs % 1000)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Leaderboard
// ---------------------------------------------------------------------------

/// A leaderboard with four labeled condsets: START, CANCEL, SUBMIT, VALUE.
///
/// Fields are `Achievement` (not bare `Condset`) so callers can access
/// `.core.conditions` and `.alts` for inspection. The evaluator only uses
/// the `.core` condset for each field.
#[derive(Debug, Clone)]
pub struct Leaderboard {
    pub id: u32,
    pub title: String,
    pub description: String,
    /// START condset — tracking begins when satisfied.
    pub start: Achievement,
    /// CANCEL condset — active run is discarded when satisfied.
    pub cancel: Achievement,
    /// SUBMIT condset — current VAL is submitted when satisfied.
    pub submit: Achievement,
    /// VALUE expression — evaluated as a pure accumulator chain (no comparison).
    pub value: Achievement,
    pub format: ValueFormat,
}

impl Leaderboard {
    /// Parse a leaderboard mem string into four `Achievement` values.
    ///
    /// Format: `STA:<condset>::CAN:<condset>::SUB:<condset>::VAL:<condset>`
    /// Parts may appear in any order; all four labels are required.
    pub fn parse(
        mem_string: &str,
    ) -> Result<(Condset, Condset, Condset, Condset), ConditionParseError> {
        let mut start: Option<Condset> = None;
        let mut cancel: Option<Condset> = None;
        let mut submit: Option<Condset> = None;
        let mut value: Option<Condset> = None;

        for part in mem_string.split("::") {
            if let Some(body) = part.strip_prefix("STA:") {
                let ach = Achievement::parse(body)?;
                start = Some(ach.core);
            } else if let Some(body) = part.strip_prefix("CAN:") {
                let ach = Achievement::parse(body)?;
                cancel = Some(ach.core);
            } else if let Some(body) = part.strip_prefix("SUB:") {
                let ach = Achievement::parse(body)?;
                submit = Some(ach.core);
            } else if let Some(body) = part.strip_prefix("VAL:") {
                let ach = Achievement::parse(body)?;
                value = Some(ach.core);
            } else {
                return Err(ConditionParseError {
                    pos: 0,
                    msg: "unknown leaderboard section label (expected STA/CAN/SUB/VAL)",
                });
            }
        }

        let start = start.ok_or(ConditionParseError { pos: 0, msg: "missing STA: section" })?;
        let cancel =
            cancel.ok_or(ConditionParseError { pos: 0, msg: "missing CAN: section" })?;
        let submit =
            submit.ok_or(ConditionParseError { pos: 0, msg: "missing SUB: section" })?;
        let value = value.ok_or(ConditionParseError { pos: 0, msg: "missing VAL: section" })?;

        Ok((start, cancel, submit, value))
    }

    /// Construct from parts plus metadata.
    pub fn from_parts(
        id: u32,
        title: impl Into<String>,
        description: impl Into<String>,
        mem_string: &str,
        format: ValueFormat,
    ) -> Result<Self, ConditionParseError> {
        let (start, cancel, submit, value) = Self::parse(mem_string)?;
        // Wrap each core condset back into an Achievement for the public field type.
        Ok(Self {
            id,
            title: title.into(),
            description: description.into(),
            start: Achievement { core: start, alts: vec![] },
            cancel: Achievement { core: cancel, alts: vec![] },
            submit: Achievement { core: submit, alts: vec![] },
            value: Achievement { core: value, alts: vec![] },
            format,
        })
    }
}

// ---------------------------------------------------------------------------
// LeaderboardEvent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderboardEvent {
    /// Leaderboard became active this frame.
    Started(u32),
    /// Leaderboard was cancelled while active.
    Cancelled(u32),
    /// Leaderboard was submitted with a value.
    Submitted { id: u32, value: i64 },
}

// ---------------------------------------------------------------------------
// LeaderboardState
// ---------------------------------------------------------------------------

/// Per-frame mutable state for a leaderboard.
#[derive(Debug, Clone, Default)]
pub struct LeaderboardState {
    pub active: bool,
    pub start_state: CondsetState,
    pub cancel_state: CondsetState,
    pub submit_state: CondsetState,
    pub value_state: CondsetState,
}

impl LeaderboardState {
    pub fn for_leaderboard(lb: &Leaderboard) -> Self {
        fn mk(c: &Condset) -> CondsetState {
            CondsetState {
                cond_states: vec![ConditionState::default(); c.conditions.len()],
                ..Default::default()
            }
        }
        Self {
            active: false,
            start_state: mk(&lb.start.core),
            cancel_state: mk(&lb.cancel.core),
            submit_state: mk(&lb.submit.core),
            value_state: mk(&lb.value.core),
        }
    }
}

// ---------------------------------------------------------------------------
// eval_leaderboard
// ---------------------------------------------------------------------------

/// Evaluate one frame of a leaderboard.
///
/// Returns a `LeaderboardEvent` on state transitions; `None` otherwise.
///
/// Order per frame:
/// 1. Evaluate START. If inactive and START fires → active, emit `Started`.
/// 2. If active, evaluate CANCEL. If fires → inactive, emit `Cancelled`.
/// 3. If active and not cancelled, evaluate SUBMIT. If fires → compute VAL,
///    go inactive, emit `Submitted`.
/// 4. If still active, evaluate VAL to keep delta snapshots fresh.
pub fn eval_leaderboard<F: FnMut(u32, MemSize) -> u32>(
    lb: &Leaderboard,
    state: &mut LeaderboardState,
    mut peek: F,
) -> Option<LeaderboardEvent> {
    // Evaluate one condset via eval_achievement. We build a fresh AchievementState
    // wrapper each call (with triggered=false) so the condset is level-triggered
    // each frame while preserving delta/prior tracking via the shared CondsetState.
    fn eval_single<F2: FnMut(u32, MemSize) -> u32>(
        condset: &Condset,
        cs: &mut CondsetState,
        peek: &mut F2,
    ) -> bool {
        let ach = Achievement { core: condset.clone(), alts: vec![] };
        let mut ast = AchievementState {
            core: cs.clone(),
            alts: vec![],
            triggered: false,
            waiting_for_trigger_flag: false,
        };
        let fired = eval_achievement(&ach, &mut ast, peek);
        *cs = ast.core;
        fired
    }

    // ------------------------------------------------------------------
    // Step 1: START
    // ------------------------------------------------------------------
    let start_fired = eval_single(&lb.start.core, &mut state.start_state, &mut peek);

    if !state.active && start_fired {
        state.active = true;
        // Reset cancel/submit/value tracking on entry into active state.
        let fresh = LeaderboardState::for_leaderboard(lb);
        state.cancel_state = fresh.cancel_state;
        state.submit_state = fresh.submit_state;
        state.value_state = fresh.value_state;
        return Some(LeaderboardEvent::Started(lb.id));
    }

    if !state.active {
        return None;
    }

    // ------------------------------------------------------------------
    // Step 2: CANCEL (only while active)
    // ------------------------------------------------------------------
    let cancel_fired = eval_single(&lb.cancel.core, &mut state.cancel_state, &mut peek);

    if cancel_fired {
        state.active = false;
        return Some(LeaderboardEvent::Cancelled(lb.id));
    }

    // ------------------------------------------------------------------
    // Step 3: SUBMIT (only while active and not cancelled)
    // ------------------------------------------------------------------
    let submit_fired = eval_single(&lb.submit.core, &mut state.submit_state, &mut peek);

    if submit_fired {
        let val = eval_value(&lb.value.core, &mut state.value_state, &mut peek);
        state.active = false;
        return Some(LeaderboardEvent::Submitted { id: lb.id, value: val });
    }

    // ------------------------------------------------------------------
    // Step 4: Keep VAL delta snapshots fresh while still active.
    // ------------------------------------------------------------------
    eval_value(&lb.value.core, &mut state.value_state, &mut peek);

    None
}

// ---------------------------------------------------------------------------
// eval_value
// ---------------------------------------------------------------------------

/// Evaluate a VAL condset as a linear accumulator and return the i64 result.
///
/// Rows are processed left-to-right:
/// - `AddSource` (A:): contribute `apply_modifier(read(lhs), op, rhs)` to accum.
/// - `SubSource` (B:): subtract the same.
/// - Any other row (Standard, Measured, etc.): add `read(lhs)` to accum.
///   This is the terminal contribution and becomes the reported score.
///
/// Delta/Prior state is tracked per-row via `ConditionState` so subsequent
/// frames see correct delta values.
fn eval_value<F: FnMut(u32, MemSize) -> u32>(
    value: &Condset,
    state: &mut CondsetState,
    peek: &mut F,
) -> i64 {
    let mut accum: i64 = 0;

    for (i, cond) in value.conditions.iter().enumerate() {
        let cs = &mut state.cond_states[i];

        // Read current memory to advance delta/prior tracking.
        let current = read_current_value(&cond.lhs, peek);
        let prev = update_delta_val(cs, current);

        let lhs_val = resolve_val_lhs(&cond.lhs, cs, prev, peek);
        let rhs_val = read_val_rhs(&cond.rhs, peek);

        match cond.flag {
            ConditionFlag::AddSource => {
                accum += apply_val_modifier(lhs_val, cond.op, rhs_val) as i64;
            }
            ConditionFlag::SubSource => {
                accum -= apply_val_modifier(lhs_val, cond.op, rhs_val) as i64;
            }
            _ => {
                // Standard / Measured / anything else: terminal row, add lhs.
                accum += lhs_val as i64;
            }
        }
    }

    accum
}

// ---------------------------------------------------------------------------
// eval_value helpers
// ---------------------------------------------------------------------------

/// Read the raw current memory value (for delta tracking).
fn read_current_value<F: FnMut(u32, MemSize) -> u32>(op: &Operand, peek: &mut F) -> u32 {
    match op {
        Operand::Memory { size, address, .. } => peek(*address, *size),
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Advance delta/prior tracking state. Returns the *previous* `last_value`
/// (correct Delta value for this frame).
fn update_delta_val(cs: &mut ConditionState, current: u32) -> u32 {
    if cs.had_sample {
        let prev = cs.last_value;
        if current != cs.last_value {
            cs.prior_value = cs.last_value;
        }
        cs.last_value = current;
        prev
    } else {
        cs.last_value = current;
        cs.prior_value = current;
        cs.had_sample = true;
        0 // first frame: delta = 0
    }
}

/// Resolve the effective LHS value, honouring Delta/Prior/BCD/Inverted modifiers.
fn resolve_val_lhs<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    prev: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { kind, size, address } => match kind {
            OperandKind::Address => peek(*address, *size),
            OperandKind::Delta => prev,
            OperandKind::Prior => cs.prior_value,
            OperandKind::Bcd => bcd_decode(peek(*address, *size)),
            OperandKind::Inverted => !peek(*address, *size) & size_mask(*size),
            OperandKind::Recall => 0,
        },
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Read the RHS operand (static, no delta state needed for VAL chains).
fn read_val_rhs<F: FnMut(u32, MemSize) -> u32>(op: &Operand, peek: &mut F) -> u32 {
    match op {
        Operand::Memory { size, address, .. } => peek(*address, *size),
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Apply a modifier operator for AddSource/SubSource rows in a VAL chain.
fn apply_val_modifier(value: u32, op: CompareOp, rhs: u32) -> u32 {
    match op {
        CompareOp::Multiply => value.wrapping_mul(rhs),
        CompareOp::Divide => {
            if rhs == 0 { 0 } else { value / rhs }
        }
        CompareOp::BitwiseAnd => value & rhs,
        CompareOp::BitwiseXor => value ^ rhs,
        CompareOp::Modulo => {
            if rhs == 0 { 0 } else { value % rhs }
        }
        _ => value, // None / comparison ops: pass raw value through
    }
}

/// Size mask for Inverted operands.
fn size_mask(size: MemSize) -> u32 {
    match size {
        MemSize::Bit0
        | MemSize::Bit1
        | MemSize::Bit2
        | MemSize::Bit3
        | MemSize::Bit4
        | MemSize::Bit5
        | MemSize::Bit6
        | MemSize::Bit7 => 0x1,
        MemSize::LowNibble | MemSize::HighNibble => 0xF,
        MemSize::Byte | MemSize::BitCount => 0xFF,
        MemSize::Word | MemSize::BigEndianWord => 0xFFFF,
        MemSize::ThreeBytes | MemSize::BigEndianThreeBytes => 0xFFFFFF,
        _ => 0xFFFFFFFF,
    }
}

/// BCD decode a packed BCD value (each nibble = one decimal digit).
fn bcd_decode(raw: u32) -> u32 {
    let d0 = raw & 0xF;
    let d1 = (raw >> 4) & 0xF;
    let d2 = (raw >> 8) & 0xF;
    let d3 = (raw >> 12) & 0xF;
    let d4 = (raw >> 16) & 0xF;
    let d5 = (raw >> 20) & 0xF;
    let d6 = (raw >> 24) & 0xF;
    let d7 = (raw >> 28) & 0xF;
    d0 + d1 * 10
        + d2 * 100
        + d3 * 1000
        + d4 * 10000
        + d5 * 100000
        + d6 * 1000000
        + d7 * 10000000
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Synthetic memory helper (mirrors evaluator's test helper)
    // -----------------------------------------------------------------------

    struct Mem(Vec<u8>);

    impl Mem {
        fn new(size: usize) -> Self {
            Self(vec![0u8; size])
        }

        fn write(&mut self, addr: usize, v: u8) {
            self.0[addr] = v;
        }

        fn peek_fn(&mut self) -> impl FnMut(u32, MemSize) -> u32 + '_ {
            move |addr, size| {
                let a = addr as usize;
                if a >= self.0.len() {
                    return 0;
                }
                match size {
                    MemSize::Byte => self.0[a] as u32,
                    MemSize::Word => {
                        let lo = self.0.get(a).copied().unwrap_or(0) as u32;
                        let hi = self.0.get(a + 1).copied().unwrap_or(0) as u32;
                        lo | (hi << 8)
                    }
                    _ => 0,
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Convenience: build a leaderboard from an inline mem string
    // -----------------------------------------------------------------------

    fn make_lb(mem_string: &str) -> Leaderboard {
        Leaderboard::from_parts(1, "Test", "Test lb", mem_string, ValueFormat::Value)
            .expect("parse")
    }

    // -----------------------------------------------------------------------
    // Test 1: Parse a full leaderboard mem string into four condsets
    // -----------------------------------------------------------------------

    #[test]
    fn parse_full_mem_string() {
        let mem = "STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=5::VAL:0xH5000";
        let (sta, can, sub, val) = Leaderboard::parse(mem).expect("parse");
        assert_eq!(sta.conditions.len(), 1);
        assert_eq!(can.conditions.len(), 1);
        assert_eq!(sub.conditions.len(), 1);
        assert_eq!(val.conditions.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 2: Parser rejects mem string missing VAL:
    // -----------------------------------------------------------------------

    #[test]
    fn parse_missing_val_errors() {
        let mem = "STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=5";
        let result = Leaderboard::parse(mem);
        assert!(result.is_err(), "should fail without VAL:");
    }

    // -----------------------------------------------------------------------
    // Test 3: ValueFormat::parse for all supported strings
    // -----------------------------------------------------------------------

    #[test]
    fn value_format_parse_all() {
        assert_eq!(ValueFormat::parse("VALUE"), ValueFormat::Value);
        assert_eq!(ValueFormat::parse("SCORE"), ValueFormat::Score);
        assert_eq!(ValueFormat::parse("FRAMES"), ValueFormat::TimeSecs);
        assert_eq!(ValueFormat::parse("TIME"), ValueFormat::TimeSecs);
        assert_eq!(ValueFormat::parse("TIMESECS"), ValueFormat::TimeSecs);
        assert_eq!(ValueFormat::parse("SECS"), ValueFormat::TimeSecs);
        assert_eq!(ValueFormat::parse("MINUTES"), ValueFormat::Minutes);
        assert_eq!(ValueFormat::parse("MILLISECS"), ValueFormat::Millisecs);
        assert_eq!(ValueFormat::parse("FIXED1"), ValueFormat::Fixed1);
        assert_eq!(ValueFormat::parse("FIXED2"), ValueFormat::Fixed2);
        assert_eq!(ValueFormat::parse("FIXED3"), ValueFormat::Fixed3);
        assert_eq!(ValueFormat::parse("UNSIGNED"), ValueFormat::Unsigned);
        assert_eq!(ValueFormat::parse("WHATEVER"), ValueFormat::Other);
    }

    // -----------------------------------------------------------------------
    // Test 4: ValueFormat::format(12345) for Score → "012345", TimeSecs → h:m:s
    // -----------------------------------------------------------------------

    #[test]
    fn format_score_six_digit() {
        assert_eq!(ValueFormat::Score.format(12345), "012345");
    }

    #[test]
    fn format_timesecs_hms() {
        // 3*3600 + 25*60 + 45 = 12345
        assert_eq!(ValueFormat::TimeSecs.format(12345), "3h25:45");
    }

    // -----------------------------------------------------------------------
    // Test 5: ValueFormat::format for Fixed2, Frames, Minutes
    // -----------------------------------------------------------------------

    #[test]
    fn format_fixed2() {
        // 12345 → "123.45"
        assert_eq!(ValueFormat::Fixed2.format(12345), "123.45");
    }

    #[test]
    fn format_frames() {
        // 3600 frames / 60 fps = 60 seconds = 1:00
        assert_eq!(ValueFormat::Frames.format(3600), "1:00");
        // 7200 frames = 120 seconds = 2:00
        assert_eq!(ValueFormat::Frames.format(7200), "2:00");
    }

    #[test]
    fn format_minutes() {
        // 65 minutes → 1h05
        assert_eq!(ValueFormat::Minutes.format(65), "1h05");
    }

    // -----------------------------------------------------------------------
    // Test 6: START trigger emits Started event
    // -----------------------------------------------------------------------

    #[test]
    fn start_trigger_emits_started() {
        let lb = make_lb("STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=5::VAL:0xH5000");
        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        // Frame 1: START condition false
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev, None);
        assert!(!state.active);

        // Frame 2: set mem[0x1234]=1 → START fires
        mem.write(0x1234, 1);
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev, Some(LeaderboardEvent::Started(1)));
        assert!(state.active);
    }

    // -----------------------------------------------------------------------
    // Test 7: SUBMIT trigger while active emits Submitted with VAL value
    // -----------------------------------------------------------------------

    #[test]
    fn submit_trigger_emits_submitted_with_val() {
        let lb = make_lb("STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=5::VAL:0xH5000");
        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        // Activate leaderboard
        mem.write(0x1234, 1);
        eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert!(state.active);

        // Set VAL address and the submit condition
        mem.write(0x5000, 42);
        mem.write(0x1235, 5); // SUB: mem[0x1235]=5

        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev, Some(LeaderboardEvent::Submitted { id: 1, value: 42 }));
        assert!(!state.active);
    }

    // -----------------------------------------------------------------------
    // Test 8: CANCEL trigger while active emits Cancelled
    // -----------------------------------------------------------------------

    #[test]
    fn cancel_trigger_emits_cancelled() {
        let lb = make_lb("STA:0xH1234=1::CAN:0xH1235=1::SUB:0xH1236=1::VAL:0xH5000");
        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        // Activate
        mem.write(0x1234, 1);
        eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert!(state.active);

        // CAN: mem[0x1235]=1
        mem.write(0x1235, 1);
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev, Some(LeaderboardEvent::Cancelled(1)));
        assert!(!state.active);
    }

    // -----------------------------------------------------------------------
    // Test 9: VAL chain with a single operand reports the operand's read value
    // -----------------------------------------------------------------------

    #[test]
    fn val_single_operand() {
        let (_, _, _, val) =
            Leaderboard::parse("STA:0xH0000=1::CAN:0xH0000=0::SUB:0xH0000=1::VAL:0xH5000")
                .unwrap();
        let mut cs = CondsetState {
            cond_states: vec![ConditionState::default(); val.conditions.len()],
            ..Default::default()
        };
        let mut mem = Mem::new(0x10000);
        mem.write(0x5000, 77);
        let v = eval_value(&val, &mut cs, &mut mem.peek_fn());
        assert_eq!(v, 77);
    }

    // -----------------------------------------------------------------------
    // Test 10: VAL chain with AddSource reports the sum
    // -----------------------------------------------------------------------

    #[test]
    fn val_add_source_sum() {
        // A:0xH5000_0xH5001 → mem[0x5000] + mem[0x5001]
        let (_, _, _, val) = Leaderboard::parse(
            "STA:0xH0000=1::CAN:0xH0000=0::SUB:0xH0000=1::VAL:A:0xH5000_0xH5001",
        )
        .unwrap();
        let mut cs = CondsetState {
            cond_states: vec![ConditionState::default(); val.conditions.len()],
            ..Default::default()
        };
        let mut mem = Mem::new(0x10000);
        mem.write(0x5000, 30);
        mem.write(0x5001, 12);
        let v = eval_value(&val, &mut cs, &mut mem.peek_fn());
        assert_eq!(v, 42);
    }

    // -----------------------------------------------------------------------
    // Test 11: VAL chain with SubSource handles subtraction
    // -----------------------------------------------------------------------

    #[test]
    fn val_sub_source() {
        // A:0xH5000_B:0xH5001_0xH5002 → accum = mem[5000] - mem[5001] + mem[5002]
        let (_, _, _, val) = Leaderboard::parse(
            "STA:0xH0000=1::CAN:0xH0000=0::SUB:0xH0000=1::VAL:A:0xH5000_B:0xH5001_0xH5002",
        )
        .unwrap();
        let mut cs = CondsetState {
            cond_states: vec![ConditionState::default(); val.conditions.len()],
            ..Default::default()
        };
        let mut mem = Mem::new(0x10000);
        mem.write(0x5000, 100); // +100
        mem.write(0x5001, 30);  // -30
        mem.write(0x5002, 5);   // terminal +5
        let v = eval_value(&val, &mut cs, &mut mem.peek_fn());
        assert_eq!(v, 75); // 100 - 30 + 5 = 75
    }

    // -----------------------------------------------------------------------
    // Test 12: Second START while already active does not re-emit
    // -----------------------------------------------------------------------

    #[test]
    fn second_start_while_active_does_not_re_emit() {
        let lb = make_lb("STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=5::VAL:0xH5000");
        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        // Activate
        mem.write(0x1234, 1);
        let ev1 = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev1, Some(LeaderboardEvent::Started(1)));

        // Next frame: START still true, already active — no re-emit
        let ev2 = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev2, None);
        assert!(state.active);
    }

    // -----------------------------------------------------------------------
    // Test 13: After SUBMIT, state resets so fresh START can re-trigger
    // -----------------------------------------------------------------------

    #[test]
    fn after_submit_fresh_start_can_retrigger() {
        let lb = make_lb("STA:0xH1234=1::CAN:0xH1234=0::SUB:0xH1235=1::VAL:0xH5000");
        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        // Activate
        mem.write(0x1234, 1);
        eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert!(state.active);

        // Submit
        mem.write(0x1235, 1);
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert!(matches!(ev, Some(LeaderboardEvent::Submitted { .. })));
        assert!(!state.active);

        // Reset conditions; go inactive so START can re-fire
        mem.write(0x1235, 0);
        mem.write(0x1234, 0);
        eval_leaderboard(&lb, &mut state, mem.peek_fn()); // idle frame

        // START fires again
        mem.write(0x1234, 1);
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn());
        assert_eq!(ev, Some(LeaderboardEvent::Started(1)));
    }

    // -----------------------------------------------------------------------
    // Test 14: Format-dispatched output for a submitted run
    // -----------------------------------------------------------------------

    #[test]
    fn format_dispatched_submitted_run() {
        let lb = Leaderboard::from_parts(
            7,
            "Speed Run",
            "Complete the level",
            "STA:0xH0000=1::CAN:0xH0000=0::SUB:0xH0001=1::VAL:0xH5000",
            ValueFormat::TimeSecs,
        )
        .expect("parse");

        let mut state = LeaderboardState::for_leaderboard(&lb);
        let mut mem = Mem::new(0x10000);

        mem.write(0x5000, 90); // 90 seconds = 1:30

        mem.write(0x0000, 1);
        eval_leaderboard(&lb, &mut state, mem.peek_fn()); // start

        mem.write(0x0001, 1);
        let ev = eval_leaderboard(&lb, &mut state, mem.peek_fn()); // submit

        if let Some(LeaderboardEvent::Submitted { id, value }) = ev {
            assert_eq!(id, 7);
            let display = lb.format.format(value);
            assert_eq!(display, "1:30"); // 90 seconds = 1:30
        } else {
            panic!("expected Submitted, got {:?}", ev);
        }
    }

    // -----------------------------------------------------------------------
    // Bonus: parse with sections in non-standard order still works
    // -----------------------------------------------------------------------

    #[test]
    fn parse_sections_any_order() {
        let mem = "VAL:0xH5000::STA:0xH1234=1::SUB:0xH1235=5::CAN:0xH1234=0";
        let result = Leaderboard::parse(mem);
        assert!(result.is_ok(), "should accept any section order");
    }
}
