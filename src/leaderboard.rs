//! Leaderboard condset wiring.
//!
//! Each leaderboard = START, CANCEL, SUBMIT, VALUE condsets. Reuses the
//! evaluator and emits state-transition events.

use crate::condition::{Achievement, CompareOp, ConditionFlag, ConditionParseError, Condset};
use crate::evaluator::{
    AchievementState, ConditionState, CondsetState, TypedValue, eval_achievement,
    base_read_size, transform_memref_value, bcd_decode, size_mask,
    is_float_size, wrap_value,
};
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
            waiting: false, // leaderboard sub-triggers skip WAITING
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
pub fn eval_value<F: FnMut(u32, MemSize) -> u32>(
    value: &Condset,
    state: &mut CondsetState,
    peek: &mut F,
) -> i64 {
    // ---------------------------------------------------------------
    // Phase 1: Advance delta/prior tracking for ALL conditions and
    //          evaluate PauseIf. rcheevos processes pause first.
    // ---------------------------------------------------------------
    let mut is_paused = false;
    let mut deltas: Vec<u32> = Vec::with_capacity(value.conditions.len());

    for (i, cond) in value.conditions.iter().enumerate() {
        let cs = &mut state.cond_states[i];
        let current = read_current_value(&cond.lhs, 0, peek);
        let prev = update_delta_val(cs, current);
        deltas.push(prev);

        if cond.flag == ConditionFlag::PauseIf {
            let lhs = resolve_val_lhs(&cond.lhs, cs, prev, TypedValue::Unsigned(0), 0, peek);
            let rhs = read_val_rhs(&cond.rhs, 0, peek);
            if compare_typed(lhs, cond.op, rhs) {
                is_paused = true;
            }
        }
    }

    if is_paused {
        // When paused, freeze hit counts and return the last computed value.
        state.is_paused = true;
        return state.last_val_result;
    }
    state.is_paused = false;

    // ---------------------------------------------------------------
    // Phase 2: Evaluate ResetIf conditions. If any fires, zero all
    //          hit counts in this condset.
    // ---------------------------------------------------------------
    let mut was_reset = false;
    for (i, cond) in value.conditions.iter().enumerate() {
        if cond.flag == ConditionFlag::ResetIf {
            let cs = &state.cond_states[i];
            let lhs = resolve_val_lhs(&cond.lhs, cs, deltas[i], TypedValue::Unsigned(0), 0, peek);
            let rhs = read_val_rhs(&cond.rhs, 0, peek);
            if compare_typed(lhs, cond.op, rhs) {
                was_reset = true;
            }
        }
    }
    if was_reset {
        for cs in state.cond_states.iter_mut() {
            cs.hit_count = 0;
        }
    }

    // ---------------------------------------------------------------
    // Phase 3: Main value evaluation — accumulator chain, tally, etc.
    // ---------------------------------------------------------------
    let mut accum = ValAccum::Int(0);
    let mut remember_val = TypedValue::Unsigned(0);
    let mut indirect_offset: u32 = 0;
    let mut and_next: bool = true;
    let mut or_next: bool = false;
    let mut add_hits: i32 = 0;
    let mut can_measure: bool = true;
    let mut measured_from_hits = false;

    for (i, cond) in value.conditions.iter().enumerate() {
        let cs = &mut state.cond_states[i];
        let prev = deltas[i];

        // For the main pass, re-read with indirect_offset for AddAddress chains.
        // Delta/prior already advanced in phase 1, so just resolve values.
        let lhs_val = resolve_val_lhs(&cond.lhs, cs, prev, remember_val, indirect_offset, peek);
        let rhs_val = read_val_rhs(&cond.rhs, indirect_offset, peek);

        match cond.flag {
            ConditionFlag::AddSource => {
                let val = apply_val_modifier_typed(lhs_val, cond.op, rhs_val);
                accum = accum.add(val);
                indirect_offset = 0;
            }
            ConditionFlag::SubSource => {
                let val = apply_val_modifier_typed(lhs_val, cond.op, rhs_val);
                accum = accum.sub(val);
                indirect_offset = 0;
            }
            ConditionFlag::AddAddress => {
                indirect_offset = indirect_offset.wrapping_add(lhs_val.to_u32());
            }
            ConditionFlag::Remember => {
                remember_val = accum.apply_to(lhs_val);
                accum = ValAccum::Int(0);
                indirect_offset = 0;
            }
            ConditionFlag::AddHits => {
                // Evaluate condition, increment hit count if true, add to add_hits.
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                let chain_ok = if or_next { or_next || cond_true }
                               else { and_next && cond_true };
                if chain_ok {
                    cs.hit_count += 1;
                }
                add_hits += cs.hit_count as i32;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::SubHits => {
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                let chain_ok = if or_next { or_next || cond_true }
                               else { and_next && cond_true };
                if chain_ok {
                    cs.hit_count += 1;
                }
                add_hits -= cs.hit_count as i32;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::Measured if is_comparison(cond.op) => {
                // Tally mode: count hits of this comparison.
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                let chain_ok = if or_next { or_next || cond_true }
                               else { and_next && cond_true };
                if chain_ok {
                    cs.hit_count += 1;
                }
                // Total hits = own hits + add_hits (from AddHits/SubHits), clamped >= 0.
                let signed_hits = cs.hit_count as i32 + add_hits;
                let total_hits = if signed_hits >= 0 { signed_hits as u32 } else { 0 };
                add_hits = 0;
                and_next = true;
                or_next = false;
                measured_from_hits = true;
                accum = ValAccum::Int(total_hits as i64);
                indirect_offset = 0;
            }
            ConditionFlag::Standard | ConditionFlag::Measured => {
                // Value mode: contribute lhs (with modifier applied) to the
                // accumulated result. For comparison operators (=, !=, <, etc.)
                // apply_val_modifier_typed returns lhs unchanged.  For modifying
                // operators (*, /, &, etc.) it applies the operation — this is
                // essential for legacy value format (`0xhAddr*60`) where the
                // multiplier is encoded as the condition operator + RHS.
                let val = apply_val_modifier_typed(lhs_val, cond.op, rhs_val);
                accum = accum.add(val);
                indirect_offset = 0;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::MeasuredIf => {
                // MeasuredIf gates whether the measured value is reported.
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                let chain_ok = if or_next { or_next || cond_true }
                               else { and_next && cond_true };
                can_measure = can_measure && chain_ok;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::AndNext => {
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                and_next = and_next && cond_true;
            }
            ConditionFlag::OrNext => {
                let cond_true = compare_typed(lhs_val, cond.op, rhs_val);
                or_next = or_next || cond_true;
            }
            // PauseIf and ResetIf already handled in phases 1-2.
            _ => {}
        }
    }

    // Phase 4: Apply MeasuredIf gate and reset-on-tally.
    // If MeasuredIf was false, or tally was reset this frame, zero the result.
    let mut result = accum.to_i64();
    if !can_measure || (measured_from_hits && was_reset) {
        result = 0;
    }

    state.last_val_result = result;
    result
}

// ---------------------------------------------------------------------------
// eval_value helpers
// ---------------------------------------------------------------------------

/// Read the raw current memory value (for delta tracking).
/// Uses base_read_size + transform_memref_value to handle sub-byte types.
fn read_current_value<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    indirect_offset: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { size, address, .. } => {
            let addr = address.wrapping_add(indirect_offset);
            let raw = peek(addr, base_read_size(*size));
            transform_memref_value(raw, *size)
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => f.to_bits(),
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
    remember_val: TypedValue,
    indirect_offset: u32,
    peek: &mut F,
) -> TypedValue {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = address.wrapping_add(indirect_offset);
            let is_float = is_float_size(*size);
            match kind {
                OperandKind::Address => {
                    let raw = peek(addr, base_read_size(*size));
                    wrap_value(transform_memref_value(raw, *size), *size)
                }
                OperandKind::Delta => {
                    if is_float { TypedValue::Float(f32::from_bits(prev)) } else { TypedValue::Unsigned(prev) }
                }
                OperandKind::Prior => {
                    if is_float { TypedValue::Float(f32::from_bits(cs.prior_value)) } else { TypedValue::Unsigned(cs.prior_value) }
                }
                OperandKind::Bcd => {
                    let raw = peek(addr, base_read_size(*size));
                    let val = transform_memref_value(raw, *size);
                    if is_float { TypedValue::Float(f32::from_bits(val)) } else { TypedValue::Unsigned(bcd_decode(val)) }
                }
                OperandKind::Inverted => {
                    let raw = peek(addr, base_read_size(*size));
                    let val = transform_memref_value(raw, *size);
                    if is_float { TypedValue::Float(f32::from_bits(val)) } else { TypedValue::Unsigned(!val & size_mask(*size)) }
                }
                OperandKind::Recall => remember_val,
            }
        }
        Operand::ConstInt(v) => TypedValue::Unsigned(*v),
        Operand::ConstFloat(f) => TypedValue::Float(*f),
    }
}

/// Read the RHS operand (static, no delta state needed for VAL chains).
fn read_val_rhs<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    indirect_offset: u32,
    peek: &mut F,
) -> TypedValue {
    match op {
        Operand::Memory { size, address, .. } => {
            let addr = address.wrapping_add(indirect_offset);
            let raw = peek(addr, base_read_size(*size));
            wrap_value(transform_memref_value(raw, *size), *size)
        }
        Operand::ConstInt(v) => TypedValue::Unsigned(*v),
        Operand::ConstFloat(f) => TypedValue::Float(*f),
    }
}

/// Is the operator a comparison (not a modifying/arithmetic operator)?
/// Comparison ops in VALUE context cause Measured to act as a tally counter.
fn is_comparison(op: CompareOp) -> bool {
    matches!(
        op,
        CompareOp::Equal
            | CompareOp::NotEqual
            | CompareOp::Less
            | CompareOp::LessOrEqual
            | CompareOp::Greater
            | CompareOp::GreaterOrEqual
    )
}

/// Compare two typed values using a comparison operator.
fn compare_typed(lhs: TypedValue, op: CompareOp, rhs: TypedValue) -> bool {
    lhs.typed_compare(op, rhs)
}

/// Typed accumulator for leaderboard value chains.
#[derive(Clone, Copy)]
enum ValAccum {
    Int(i64),
    Float(f64),
}

impl ValAccum {
    fn add(self, val: TypedValue) -> Self {
        match (self, val) {
            (ValAccum::Int(acc), TypedValue::Unsigned(v)) => ValAccum::Int(acc + v as i64),
            (ValAccum::Int(acc), TypedValue::Float(f)) => ValAccum::Float(acc as f64 + f as f64),
            (ValAccum::Float(acc), TypedValue::Unsigned(v)) => ValAccum::Float(acc + v as f64),
            (ValAccum::Float(acc), TypedValue::Float(f)) => ValAccum::Float(acc + f as f64),
        }
    }

    fn sub(self, val: TypedValue) -> Self {
        match (self, val) {
            (ValAccum::Int(acc), TypedValue::Unsigned(v)) => ValAccum::Int(acc - v as i64),
            (ValAccum::Int(acc), TypedValue::Float(f)) => ValAccum::Float(acc as f64 - f as f64),
            (ValAccum::Float(acc), TypedValue::Unsigned(v)) => ValAccum::Float(acc - v as f64),
            (ValAccum::Float(acc), TypedValue::Float(f)) => ValAccum::Float(acc - f as f64),
        }
    }

    fn apply_to(self, raw_lhs: TypedValue) -> TypedValue {
        match self {
            ValAccum::Int(0) => raw_lhs,
            ValAccum::Int(acc) => match raw_lhs {
                TypedValue::Unsigned(v) => TypedValue::Unsigned((v as i64 + acc) as u32),
                TypedValue::Float(f) => TypedValue::Float((f as f64 + acc as f64) as f32),
            },
            ValAccum::Float(acc) => match raw_lhs {
                TypedValue::Unsigned(v) => TypedValue::Float((v as f64 + acc) as f32),
                TypedValue::Float(f) => TypedValue::Float((f as f64 + acc) as f32),
            },
        }
    }

    fn to_i64(self) -> i64 {
        match self {
            ValAccum::Int(v) => v,
            ValAccum::Float(f) => f as i64,
        }
    }
}

/// Apply a modifier operation with typed values for VAL chains.
fn apply_val_modifier_typed(lhs: TypedValue, op: CompareOp, rhs: TypedValue) -> TypedValue {
    match op {
        CompareOp::BitwiseAnd | CompareOp::BitwiseXor => {
            let a = lhs.to_u32();
            let b = rhs.to_u32();
            TypedValue::Unsigned(match op {
                CompareOp::BitwiseAnd => a & b,
                _ => a ^ b,
            })
        }
        _ => {
            if lhs.is_float() || rhs.is_float() {
                let a = lhs.to_f32();
                let b = rhs.to_f32();
                TypedValue::Float(match op {
                    CompareOp::Multiply => a * b,
                    CompareOp::Divide => if b == 0.0 { 0.0 } else { a / b },
                    CompareOp::Modulo => if b == 0.0 { 0.0 } else { a % b },
                    _ => a,
                })
            } else {
                let a = lhs.to_u32();
                let b = rhs.to_u32();
                TypedValue::Unsigned(match op {
                    CompareOp::Multiply => a.wrapping_mul(b),
                    CompareOp::Divide => if b == 0 { 0 } else { a / b },
                    CompareOp::Modulo => if b == 0 { 0 } else { a % b },
                    _ => a,
                })
            }
        }
    }
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
                let n: usize = match size {
                    MemSize::Byte => 1,
                    MemSize::Word => 2,
                    MemSize::ThreeBytes => 3,
                    MemSize::DWord => 4,
                    _ => 1,
                };
                let mut val: u32 = 0;
                for i in 0..n {
                    val |= (self.0.get(a + i).copied().unwrap_or(0) as u32) << (8 * i);
                }
                val
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
