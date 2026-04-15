//! Per-frame achievement evaluator.
//!
//! Owns all mutable state (hit counts, delta snapshots, prior values) and
//! drives one full evaluation pass each time [`eval_achievement`] is called.

use crate::condition::{Achievement, CompareOp, ConditionFlag, Condset};
use crate::memaddr::{MemSize, Operand, OperandKind};

// ---------------------------------------------------------------------------
// Public callback type alias
// ---------------------------------------------------------------------------

/// Memory-peek callback: `peek(address, size) -> value`.
///
/// The evaluator calls this to read bytes out of the emulator's address space.
/// Implementations must be side-effect free.
pub type Peek = dyn FnMut(u32, MemSize) -> u32;

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

/// Per-condition mutable state.
#[derive(Debug, Clone, Default)]
pub struct ConditionState {
    /// Number of frames this condition has been true so far.
    pub hit_count: u32,
    /// The most-recently observed value of the LHS memory operand.
    /// Used as the "delta" (previous-frame) value on the next frame.
    pub last_value: u32,
    /// Value before the most recent *change* to `last_value`.
    pub prior_value: u32,
    /// False on the very first frame so that delta returns 0.
    pub had_sample: bool,
}

/// Per-condset mutable state.
#[derive(Debug, Clone, Default)]
pub struct CondsetState {
    pub cond_states: Vec<ConditionState>,
    /// True while a PauseIf condition is currently active.
    pub is_paused: bool,
    /// Set to true on the frame when a ResetIf fired.
    pub did_reset: bool,
    /// True once the condset reaches its satisfied state.
    pub triggered: bool,
    /// Current Measured hit count (or raw value).
    pub measured: u32,
    /// Target for Measured progress.
    pub measured_target: u32,
}

/// Full achievement mutable state.
#[derive(Debug, Clone, Default)]
pub struct AchievementState {
    pub core: CondsetState,
    pub alts: Vec<CondsetState>,
    /// True once the achievement has been triggered (latched).
    pub triggered: bool,
    /// True when the achievement has passed core+alts but is waiting for the
    /// Trigger-flagged condition to fire.
    pub waiting_for_trigger_flag: bool,
}

impl AchievementState {
    /// Allocate fresh state sized to match `a`'s condset lengths.
    pub fn for_achievement(a: &Achievement) -> Self {
        let core = CondsetState {
            cond_states: vec![ConditionState::default(); a.core.conditions.len()],
            ..Default::default()
        };
        let alts = a
            .alts
            .iter()
            .map(|c| CondsetState {
                cond_states: vec![ConditionState::default(); c.conditions.len()],
                ..Default::default()
            })
            .collect();
        Self { core, alts, triggered: false, waiting_for_trigger_flag: false }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Evaluate one frame of an achievement against the current memory state.
///
/// Returns `true` on the frame the achievement first triggers.
/// Subsequent calls return `false` (the trigger is latched in `state.triggered`).
pub fn eval_achievement<F: FnMut(u32, MemSize) -> u32>(
    a: &Achievement,
    state: &mut AchievementState,
    mut peek: F,
) -> bool {
    if state.triggered {
        return false;
    }

    // ------------------------------------------------------------------
    // Check for global ResetIf: any ResetIf in any condset firing resets
    // ALL condsets in rcheevos (per-achievement reset).  We replicate that
    // behaviour by doing a first pass to detect global resets.
    // Note: PauseIf in a condset suppresses that condset's ResetIf check.
    // ------------------------------------------------------------------

    // Evaluate core condset.
    eval_condset(&a.core, &mut state.core, &mut peek);

    // Evaluate all alt condsets.
    let num_alts = a.alts.len();
    for i in 0..num_alts {
        eval_condset(&a.alts[i], &mut state.alts[i], &mut peek);
    }

    // ------------------------------------------------------------------
    // Global reset propagation: if any condset emitted a reset signal,
    // zero all hit counts across the whole achievement.
    // ------------------------------------------------------------------
    let any_reset = state.core.did_reset || state.alts.iter().any(|s| s.did_reset);
    if any_reset {
        reset_all_hits(state);
    }

    // ------------------------------------------------------------------
    // Determine whether the achievement should trigger this frame.
    // ------------------------------------------------------------------

    // Re-read core_satisfied after possible reset (if core reset, condset
    // won't be satisfied on this frame anyway since hits were just cleared,
    // but re-check to be safe).
    let core_ok = state.core.triggered;

    let alts_ok = if a.alts.is_empty() {
        true
    } else {
        state.alts.iter().any(|s| s.triggered)
    };

    if core_ok && alts_ok {
        state.triggered = true;
        state.waiting_for_trigger_flag = false;
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Per-condset evaluation
// ---------------------------------------------------------------------------

/// Evaluate one condset and update its state in place.
/// Returns `true` if the condset is satisfied this frame.
fn eval_condset<F: FnMut(u32, MemSize) -> u32>(
    condset: &Condset,
    state: &mut CondsetState,
    peek: &mut F,
) -> bool {
    // ------------------------------------------------------------------
    // Per-frame accumulators (reset at start of each condset evaluation).
    // ------------------------------------------------------------------

    // Running sum for AddSource / SubSource chains.
    let mut add_accum: i64 = 0;
    // Modifier op to apply to the accumulator contribution from this row.
    let _add_op: CompareOp = CompareOp::None;
    // True as long as the AndNext chain is passing.
    let mut and_next: bool = true;
    // True once an OrNext arm has been seen and is passing.
    let mut or_next_active: bool = false;
    // Whether we're inside an OrNext chain at all.
    let mut in_or_next: bool = false;
    // Running address offset from AddAddress rows.
    let mut indirect_offset: u32 = 0;
    // Accumulated AddHits/SubHits delta for the shared hit pool.
    let mut shared_hits_delta: i64 = 0;
    // The condition index that "owns" the shared hit pool (last non-Add/SubHits
    // condition in the chain), plus its required_hits.
    let _shared_hit_target: Option<(usize, u32)> = None;

    // ------------------------------------------------------------------
    // Phase 1: Check for PauseIf (scan entire condset first).
    // We must know whether to pause before touching hit counts.
    // PauseIf itself can have a hit target; if so it pauses only after
    // that many frames of being true.
    // ------------------------------------------------------------------
    let was_paused = state.is_paused;
    let mut new_paused = false;
    for (i, cond) in condset.conditions.iter().enumerate() {
        if cond.flag != ConditionFlag::PauseIf {
            continue;
        }
        let cs = &mut state.cond_states[i];
        // Always peek the raw current memory so delta/prior state stays accurate.
        let raw_current = peek_current(&cond.lhs, indirect_offset, peek);
        let prev_lhs = raw_current.map_or(0, |curr| update_delta(cs, curr));
        let lhs_val = resolve_comparison_lhs(&cond.lhs, cs, prev_lhs, indirect_offset, peek);
        let rhs_val = read_operand_static(&cond.rhs, peek);
        let passes = compare(lhs_val, cond.op, rhs_val);
        if passes {
            if cond.required_hits == 0 {
                new_paused = true;
            } else {
                // PauseIf with hit target: pause once hit count reaches target.
                // Advance hit count only if not already paused.
                if !was_paused {
                    cs.hit_count += 1;
                }
                if cs.hit_count >= cond.required_hits {
                    new_paused = true;
                }
            }
        }
    }
    state.is_paused = new_paused;
    state.did_reset = false;

    if state.is_paused {
        // When paused, do not advance any hit counts (including ResetIf).
        // Mark condset as not triggered.
        state.triggered = false;
        return false;
    }

    // ------------------------------------------------------------------
    // Phase 2: ResetNextIf pre-scan to know which conditions should have
    // their hits zeroed this frame before we process them.
    // We track which condition index follows each ResetNextIf.
    // ------------------------------------------------------------------
    let mut reset_next_set: Vec<bool> = vec![false; condset.conditions.len()];
    {
        let n = condset.conditions.len();
        let mut i = 0;
        while i < n {
            let cond = &condset.conditions[i];
            if cond.flag == ConditionFlag::ResetNextIf {
                let cs = &state.cond_states[i];
                // For the pre-scan we only need the current comparison value.
                let lhs_val = resolve_comparison_lhs_static(&cond.lhs, cs, indirect_offset, peek);
                let rhs_val = read_operand_static(&cond.rhs, peek);
                if compare(lhs_val, cond.op, rhs_val) && i + 1 < n {
                    reset_next_set[i + 1] = true;
                }
            }
            i += 1;
        }
    }

    // ------------------------------------------------------------------
    // Phase 3: Main walk.
    // ------------------------------------------------------------------

    // Track whether every Standard/Measured/Trigger condition is satisfied.
    // A condset is satisfied iff all "terminal" conditions pass their hit targets.
    let mut all_satisfied = true;
    let mut has_trigger = false;
    let mut trigger_satisfied = true;
    let mut _remember_val: u32 = 0; // TODO: Recall/Remember register (single slot)

    let n = condset.conditions.len();
    let mut i = 0;
    while i < n {
        let cond = &condset.conditions[i];
        let cs = &mut state.cond_states[i];

        // Apply ResetNextIf: zero this condition's hit count.
        if reset_next_set[i] {
            cs.hit_count = 0;
        }

        // Step 1: Peek the raw current memory value for delta/prior tracking.
        let raw_current = peek_current(&cond.lhs, indirect_offset, peek);
        // Step 2: Update delta/prior state; returns the previous last_value
        // (which is the correct "delta" comparison value for this frame).
        let prev_lhs = raw_current.map_or(0, |curr| update_delta(cs, curr));
        // Step 3: Resolve the actual comparison value from the operand kind.
        let raw_lhs = resolve_comparison_lhs(&cond.lhs, cs, prev_lhs, indirect_offset, peek);
        // RHS uses the same condition state so that Delta/Prior on the RHS
        // (e.g. `0xH0000>d0xH0000`) returns the correct previous-frame value.
        let raw_rhs = read_operand_rhs(&cond.rhs, cs, prev_lhs, peek);

        match cond.flag {
            ConditionFlag::PauseIf => {
                // Already handled above; skip hit-count bookkeeping here.
                i += 1;
                continue;
            }
            ConditionFlag::ResetNextIf => {
                // Already handled in pre-scan; skip normal path.
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AddSource: accumulate into add_accum, do not compare.
            // ------------------------------------------------------------------
            ConditionFlag::AddSource => {
                let val = apply_modifier(raw_lhs, cond.op, raw_rhs);
                add_accum += val as i64;
                indirect_offset = 0;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // SubSource: subtract from add_accum.
            // ------------------------------------------------------------------
            ConditionFlag::SubSource => {
                let val = apply_modifier(raw_lhs, cond.op, raw_rhs);
                add_accum -= val as i64;
                indirect_offset = 0;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AddAddress: accumulate address offset.
            // ------------------------------------------------------------------
            ConditionFlag::AddAddress => {
                let val = apply_modifier(raw_lhs, cond.op, raw_rhs);
                indirect_offset = indirect_offset.wrapping_add(val);
                // Do NOT reset indirect_offset here — it carries forward.
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Remember: store LHS into the remember register.
            // TODO: Recall operand support is stubbed — the Remember register is
            // captured but {recall} operands are not yet handled in read_operand.
            // ------------------------------------------------------------------
            ConditionFlag::Remember => {
                _remember_val = raw_lhs;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AndNext: fold comparison result into the and_next gate.
            // ------------------------------------------------------------------
            ConditionFlag::AndNext => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;
                let passes = compare(effective_lhs, cond.op, raw_rhs);
                if in_or_next {
                    or_next_active = or_next_active || passes;
                } else {
                    and_next = and_next && passes;
                }
                in_or_next = false;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // OrNext: fold into or_next.
            // ------------------------------------------------------------------
            ConditionFlag::OrNext => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;
                let passes = compare(effective_lhs, cond.op, raw_rhs);
                if !in_or_next {
                    // Start of an OrNext chain. The previous and_next result
                    // carries over and the first arm seeds or_next_active.
                    in_or_next = true;
                    or_next_active = passes;
                } else {
                    or_next_active = or_next_active || passes;
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AddHits: contribute hits toward shared pool, then continue.
            // ------------------------------------------------------------------
            ConditionFlag::AddHits => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;

                let cmp = compare(effective_lhs, cond.op, raw_rhs);
                let passes = apply_gate(cmp, and_next, in_or_next, or_next_active);
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);
                if passes {
                    cs.hit_count += 1;
                }
                shared_hits_delta += cs.hit_count as i64;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // SubHits: subtract hits from shared pool.
            // TODO: SubHits is implemented at the hit-pool level but the
            // interaction with combined AddHits/Standard rows is subtle and
            // may not match rcheevos exactly in edge cases.
            // ------------------------------------------------------------------
            ConditionFlag::SubHits => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;

                let cmp = compare(effective_lhs, cond.op, raw_rhs);
                let passes = apply_gate(cmp, and_next, in_or_next, or_next_active);
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);
                if passes {
                    cs.hit_count += 1;
                }
                shared_hits_delta -= cs.hit_count as i64;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // ResetIf: zero all hit counts when true.
            // ------------------------------------------------------------------
            ConditionFlag::ResetIf => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                if compare(effective_lhs, cond.op, raw_rhs) {
                    state.did_reset = true;
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Measured: count hits (or raw value) and expose as progress.
            // ------------------------------------------------------------------
            ConditionFlag::Measured => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;

                let cmp = compare(effective_lhs, cond.op, raw_rhs);
                let passes = apply_gate(cmp, and_next, in_or_next, or_next_active);
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);
                if passes {
                    cs.hit_count += 1;
                }
                state.measured = cs.hit_count;
                state.measured_target = cond.required_hits;

                if cond.required_hits > 0 {
                    // Hit-count mode: satisfies when hit target met.
                    if cs.hit_count < cond.required_hits {
                        all_satisfied = false;
                    }
                } else {
                    // Value mode: measured always satisfies (it reports, not gates).
                    // A bare `M:0xH0000>=5` with no hit count is always satisfied.
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // MeasuredIf: gates whether Measured is reported. Does NOT affect
            // triggering — it only suppresses the measured progress display.
            // TODO: In rcheevos, if MeasuredIf is false the Measured value is
            // hidden from the display but the condset can still trigger. We
            // capture this semantics partially — the `measured` field will still
            // be updated regardless. A proper implementation would zero
            // `state.measured` when MeasuredIf fails. Stubbed for now.
            // ------------------------------------------------------------------
            ConditionFlag::MeasuredIf => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                // MeasuredIf does not contribute to satisfied/not-satisfied on its own.
                // The condition must be true for Measured to be shown; if false
                // we could zero state.measured but we leave that for a later pass.
                let _passes = compare(effective_lhs, cond.op, raw_rhs);
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Trigger: like Standard but the achievement is "primed" until this
            // condition fires. If there is a Trigger condition, the achievement
            // does not trigger until this condition passes its hit target.
            // ------------------------------------------------------------------
            ConditionFlag::Trigger => {
                has_trigger = true;
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;

                let cmp = compare(effective_lhs, cond.op, raw_rhs);
                let passes = apply_gate(cmp, and_next, in_or_next, or_next_active);
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);
                if passes {
                    if cond.required_hits == 0 {
                        cs.hit_count = 1; // treat as "triggered now"
                    } else {
                        cs.hit_count += 1;
                    }
                }
                let trigger_done = if cond.required_hits == 0 {
                    passes
                } else {
                    cs.hit_count >= cond.required_hits
                };
                if !trigger_done {
                    trigger_satisfied = false;
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Standard: normal comparison condition.
            // ------------------------------------------------------------------
            ConditionFlag::Standard => {
                let effective_lhs = (raw_lhs as i64 + add_accum) as u32;
                add_accum = 0;
                indirect_offset = 0;

                let cmp = compare(effective_lhs, cond.op, raw_rhs);
                let passes = apply_gate(cmp, and_next, in_or_next, or_next_active);
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                // Check if this condition is the target of a pending AddHits pool.
                // A pool is active if shared_hits_delta != 0 and this is not an
                // AddHits/SubHits row. We add the shared delta to this condition's
                // hit count for the purpose of satisfying the target.
                let effective_hits = if shared_hits_delta != 0 {
                    let total = cs.hit_count as i64 + shared_hits_delta;
                    shared_hits_delta = 0; // pool consumed
                    total.max(0) as u32
                } else {
                    if passes {
                        cs.hit_count += 1;
                    }
                    cs.hit_count
                };

                if cond.required_hits == 0 {
                    // Level-triggered: must be true right now.
                    if !passes {
                        all_satisfied = false;
                    }
                } else {
                    if effective_hits < cond.required_hits {
                        all_satisfied = false;
                    }
                    // Keep hit_count in sync with effective (AddHits case).
                    if shared_hits_delta == 0 {
                        cs.hit_count = effective_hits;
                    }
                }
            }
        }

        i += 1;
    }

    // If we have a Trigger flag and it didn't satisfy, the condset is not triggered.
    let final_satisfied = all_satisfied && (!has_trigger || trigger_satisfied);
    state.triggered = final_satisfied;
    final_satisfied
}

// ---------------------------------------------------------------------------
// Helper: reset all hit counts across the whole achievement
// ---------------------------------------------------------------------------

fn reset_all_hits(state: &mut AchievementState) {
    for cs in &mut state.core.cond_states {
        cs.hit_count = 0;
    }
    for condset_state in &mut state.alts {
        for cs in &mut condset_state.cond_states {
            cs.hit_count = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for AndNext/OrNext gate management
// ---------------------------------------------------------------------------

/// Compute whether a terminal condition "passes" given the accumulated chain state
/// and its own raw comparison result.
///
/// - If `in_or_next`: the terminal condition is the last arm of an OR chain.
///   `passes = or_next_active || raw_passes`.  The `and_next` accumulated so far
///   is factored in as a multiplier over the whole OR group, but for simplicity
///   (and matching rcheevos) we gate the whole OR group with `and_next`.
/// - Otherwise: `passes = raw_passes && and_next`.
fn apply_gate(raw_passes: bool, and_next: bool, in_or_next: bool, or_next_active: bool) -> bool {
    if in_or_next {
        // Terminal of an OrNext chain: OR with accumulated arms, then AND with and_next.
        (raw_passes || or_next_active) && and_next
    } else {
        raw_passes && and_next
    }
}

/// Reset and_next to `true` and clear the OrNext chain after a terminal row.
fn reset_and_or(and_next: &mut bool, in_or_next: &mut bool, or_next_active: &mut bool) {
    *and_next = true;
    *in_or_next = false;
    *or_next_active = false;
}

// ---------------------------------------------------------------------------
// Operand evaluation
// ---------------------------------------------------------------------------

/// Peek the raw current memory value for the address component of a Memory operand.
/// Returns `None` for non-Memory operands.
/// This value is used ONLY to update delta/prior state.
fn peek_current<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    indirect_offset: u32,
    peek: &mut F,
) -> Option<u32> {
    match op {
        Operand::Memory { size, address, .. } => {
            let addr = address.wrapping_add(indirect_offset);
            Some(peek(addr, *size))
        }
        _ => None,
    }
}

/// Resolve the actual comparison value for the LHS operand.
///
/// `prev_lhs` is the value returned by `update_delta` (the previous frame's
/// `last_value`, or 0 on the first frame).  This is the correct Delta value.
///
/// For Address operands we re-peek here (cheap, avoids storing the current value
/// separately).  For Delta we use `prev_lhs`.  For Prior we read `cs.prior_value`
/// (which update_delta already advanced correctly).
fn resolve_comparison_lhs<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    prev_lhs: u32,
    indirect_offset: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = address.wrapping_add(indirect_offset);
            match kind {
                OperandKind::Address => peek(addr, *size),
                OperandKind::Delta => prev_lhs,
                OperandKind::Prior => cs.prior_value,
                OperandKind::Bcd => bcd_decode(peek(addr, *size)),
                OperandKind::Inverted => !peek(addr, *size) & size_mask(*size),
                OperandKind::Recall => {
                    // TODO: Recall operand reads the Remember register.
                    // Full implementation requires threading the remember_val;
                    // stubbed to 0 for now.
                    0
                }
            }
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Like `resolve_comparison_lhs` but without delta tracking (for pre-scans that
/// only need the current comparison value without advancing state).
fn resolve_comparison_lhs_static<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    indirect_offset: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = address.wrapping_add(indirect_offset);
            match kind {
                OperandKind::Address | OperandKind::Bcd | OperandKind::Inverted => {
                    peek(addr, *size)
                }
                OperandKind::Delta => {
                    if cs.had_sample { cs.last_value } else { 0 }
                }
                OperandKind::Prior => cs.prior_value,
                OperandKind::Recall => 0,
            }
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Read the RHS operand value, using the condition's own state for delta/prior.
///
/// When the RHS is `d0xHaddr` or `p0xHaddr`, we use `cs` (the LHS condition's state)
/// to supply the delta/prior value.  This is correct whenever LHS and RHS reference
/// the same address — the common case in rcheevos.  For conditions where RHS is a
/// *different* address with a delta/prior modifier, the result is approximate; a full
/// implementation would require per-address state tracking.
fn read_operand_rhs<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    prev_lhs: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = *address;
            match kind {
                OperandKind::Address => peek(addr, *size),
                OperandKind::Delta => prev_lhs,
                OperandKind::Prior => cs.prior_value,
                OperandKind::Bcd => bcd_decode(peek(addr, *size)),
                OperandKind::Inverted => !peek(addr, *size) & size_mask(*size),
                OperandKind::Recall => 0, // TODO
            }
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Read an operand when we have no associated condition state.
/// For memory operands this always reads the current value (no delta/prior).
/// Used for PauseIf pre-scan and static RHS without delta context.
fn read_operand_static<F: FnMut(u32, MemSize) -> u32>(op: &Operand, peek: &mut F) -> u32 {
    match op {
        Operand::Memory { kind: _, size, address } => peek(*address, *size),
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => *f as u32,
    }
}

/// Update `last_value` and `prior_value` from a freshly read `current` value.
/// Returns the *previous* last_value (used to supply the delta comparison value
/// on the same frame, before the state advances).
fn update_delta(cs: &mut ConditionState, current: u32) -> u32 {
    if cs.had_sample {
        let prev = cs.last_value;
        if current != cs.last_value {
            cs.prior_value = cs.last_value;
        }
        cs.last_value = current;
        prev
    } else {
        // First frame: seed last_value and prior_value to current.
        // Delta will return current value on frame 1 (prev == current).
        cs.last_value = current;
        cs.prior_value = current;
        cs.had_sample = true;
        // On first frame, delta returns 0 per rcheevos convention.
        // We signal this by returning 0 (caller uses the return value for Delta).
        0
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

fn compare(lhs: u32, op: CompareOp, rhs: u32) -> bool {
    match op {
        CompareOp::Equal => lhs == rhs,
        CompareOp::NotEqual => lhs != rhs,
        CompareOp::Less => lhs < rhs,
        CompareOp::LessOrEqual => lhs <= rhs,
        CompareOp::Greater => lhs > rhs,
        CompareOp::GreaterOrEqual => lhs >= rhs,
        // Modifier ops used on accumulator rows are not comparisons.
        CompareOp::Multiply
        | CompareOp::Divide
        | CompareOp::BitwiseAnd
        | CompareOp::BitwiseXor
        | CompareOp::Modulo
        | CompareOp::None => false,
    }
}

/// Apply a modifier operation to `value` scaled by `rhs` (for Add/SubSource chains).
/// Returns the (possibly scaled/masked) contribution to the accumulator.
fn apply_modifier(value: u32, op: CompareOp, rhs: u32) -> u32 {
    match op {
        CompareOp::Multiply => value.wrapping_mul(rhs),
        CompareOp::Divide => {
            if rhs == 0 {
                0
            } else {
                value / rhs
            }
        }
        CompareOp::BitwiseAnd => value & rhs,
        CompareOp::BitwiseXor => value ^ rhs,
        CompareOp::Modulo => {
            if rhs == 0 {
                0
            } else {
                value % rhs
            }
        }
        // For None / comparison ops: just pass the raw value through unchanged.
        _ => value,
    }
}

// ---------------------------------------------------------------------------
// Size mask (for Inverted operand)
// ---------------------------------------------------------------------------

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
        MemSize::DWord
        | MemSize::BigEndianDWord
        | MemSize::Float
        | MemSize::FloatBigEndian
        | MemSize::MbfFloat
        | MemSize::MbfFloatLittleEndian
        | MemSize::Double32
        | MemSize::Double32BigEndian => 0xFFFFFFFF,
    }
}

// ---------------------------------------------------------------------------
// BCD decode
// ---------------------------------------------------------------------------

fn bcd_decode(raw: u32) -> u32 {
    // Treat the raw value as packed BCD (each nibble is one decimal digit).
    let d0 = (raw & 0xF) as u32;
    let d1 = ((raw >> 4) & 0xF) as u32;
    let d2 = ((raw >> 8) & 0xF) as u32;
    let d3 = ((raw >> 12) & 0xF) as u32;
    let d4 = ((raw >> 16) & 0xF) as u32;
    let d5 = ((raw >> 20) & 0xF) as u32;
    let d6 = ((raw >> 24) & 0xF) as u32;
    let d7 = ((raw >> 28) & 0xF) as u32;
    d0
        + d1 * 10
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
    use crate::condition::Achievement;
    use crate::memaddr::MemSize;

    // -----------------------------------------------------------------------
    // Synthetic memory helper
    // -----------------------------------------------------------------------

    struct Mem(Vec<u8>);

    impl Mem {
        fn new(size: usize) -> Self {
            Self(vec![0u8; size])
        }

        fn write(&mut self, addr: usize, v: u8) {
            self.0[addr] = v;
        }

        fn write_u16_le(&mut self, addr: usize, v: u16) {
            let b = v.to_le_bytes();
            self.0[addr] = b[0];
            self.0[addr + 1] = b[1];
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
                    MemSize::ThreeBytes => {
                        let b0 = self.0.get(a).copied().unwrap_or(0) as u32;
                        let b1 = self.0.get(a + 1).copied().unwrap_or(0) as u32;
                        let b2 = self.0.get(a + 2).copied().unwrap_or(0) as u32;
                        b0 | (b1 << 8) | (b2 << 16)
                    }
                    MemSize::DWord => {
                        let b0 = self.0.get(a).copied().unwrap_or(0) as u32;
                        let b1 = self.0.get(a + 1).copied().unwrap_or(0) as u32;
                        let b2 = self.0.get(a + 2).copied().unwrap_or(0) as u32;
                        let b3 = self.0.get(a + 3).copied().unwrap_or(0) as u32;
                        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
                    }
                    MemSize::LowNibble => (self.0[a] & 0x0F) as u32,
                    MemSize::HighNibble => ((self.0[a] >> 4) & 0x0F) as u32,
                    MemSize::Bit0 => ((self.0[a] >> 0) & 1) as u32,
                    MemSize::Bit1 => ((self.0[a] >> 1) & 1) as u32,
                    MemSize::Bit2 => ((self.0[a] >> 2) & 1) as u32,
                    MemSize::Bit3 => ((self.0[a] >> 3) & 1) as u32,
                    MemSize::Bit4 => ((self.0[a] >> 4) & 1) as u32,
                    MemSize::Bit5 => ((self.0[a] >> 5) & 1) as u32,
                    MemSize::Bit6 => ((self.0[a] >> 6) & 1) as u32,
                    MemSize::Bit7 => ((self.0[a] >> 7) & 1) as u32,
                    MemSize::BitCount => self.0[a].count_ones(),
                    _ => 0,
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Convenience runner: parse achievement, run N frames, return trigger frame
    // -----------------------------------------------------------------------

    /// Run up to `max_frames`, calling `per_frame` to modify memory before each.
    /// Returns the 1-based frame number on which the achievement triggered, or None.
    fn run_frames<F>(src: &str, max_frames: usize, mut per_frame: F) -> Option<usize>
    where
        F: FnMut(usize, &mut Mem),
    {
        let ach = Achievement::parse(src).expect("parse");
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        for frame in 1..=max_frames {
            per_frame(frame, &mut mem);
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                return Some(frame);
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Test 1: Simple equality trigger
    // -----------------------------------------------------------------------

    #[test]
    fn simple_equality_triggers() {
        let trigger = run_frames("0xH1234=5", 5, |frame, mem| {
            if frame == 3 {
                mem.write(0x1234, 5);
            }
        });
        assert_eq!(trigger, Some(3));
    }

    #[test]
    fn simple_equality_no_trigger_if_never_true() {
        let trigger = run_frames("0xH1234=5", 10, |_frame, mem| {
            mem.write(0x1234, 7);
        });
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 2: Hit count — must be true for N frames
    // -----------------------------------------------------------------------

    #[test]
    fn hit_count_3_frames() {
        // Condition true from frame 1; should trigger on frame 3.
        let trigger = run_frames("0xH1234=5(3)", 10, |_frame, mem| {
            mem.write(0x1234, 5);
        });
        assert_eq!(trigger, Some(3));
    }

    #[test]
    fn hit_count_not_met() {
        // True for only 2 frames, need 3.
        let trigger = run_frames("0xH1234=5(3)", 10, |frame, mem| {
            if frame <= 2 {
                mem.write(0x1234, 5);
            } else {
                mem.write(0x1234, 0);
            }
        });
        assert_eq!(trigger, None);
    }

    #[test]
    fn hit_count_legacy_dot_form() {
        let trigger = run_frames("0xH1234=5.3.", 10, |_frame, mem| {
            mem.write(0x1234, 5);
        });
        assert_eq!(trigger, Some(3));
    }

    // -----------------------------------------------------------------------
    // Test 3: PauseIf blocks hit counting
    // -----------------------------------------------------------------------

    #[test]
    fn pauseif_blocks_hits() {
        // P:0xH0001=1 pauses while mem[1]=1
        // 0xH0000=1(3) needs 3 hits; mem[0]=1 always, but paused for first 5 frames.
        let ach = Achievement::parse("P:0xH0001=1_0xH0000=1(3)").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1);
        mem.write(0x0001, 1); // paused initially

        let mut triggered_frame = None;
        for frame in 1..=15usize {
            if frame == 6 {
                mem.write(0x0001, 0); // unpause on frame 6
            }
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                triggered_frame = Some(frame);
                break;
            }
        }
        // Hits can only accumulate from frame 6 onward; trigger on frame 8.
        assert_eq!(triggered_frame, Some(8));
    }

    // -----------------------------------------------------------------------
    // Test 4: ResetIf zeros hit counts
    // -----------------------------------------------------------------------

    #[test]
    fn resetif_clears_hits() {
        // 0xH0000=1(5) needs 5 hits; R:0xH0001=1 resets on frame 3.
        let ach = Achievement::parse("0xH0000=1(5)_R:0xH0001=1").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1); // main condition always true

        let mut triggered_frame = None;
        for frame in 1..=15usize {
            if frame == 3 {
                mem.write(0x0001, 1); // trigger reset
            } else {
                mem.write(0x0001, 0);
            }
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                triggered_frame = Some(frame);
                break;
            }
        }
        // After reset on frame 3, need 5 more frames: triggers on frame 3+5=8.
        assert_eq!(triggered_frame, Some(8));
    }

    // -----------------------------------------------------------------------
    // Test 5: AddSource chain mem[0]+mem[1]==10
    // -----------------------------------------------------------------------

    #[test]
    fn add_source_chain() {
        // A:0xH0000_0xH0001=10  ← mem[0]+mem[1] == 10
        let trigger = run_frames("A:0xH0000_0xH0001=10", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 3);
                mem.write(0x0001, 7); // 3+7=10
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn add_source_chain_false() {
        let trigger = run_frames("A:0xH0000_0xH0001=10", 5, |_frame, mem| {
            mem.write(0x0000, 3);
            mem.write(0x0001, 8); // 3+8=11 != 10
        });
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 6: SubSource chain
    // -----------------------------------------------------------------------

    #[test]
    fn sub_source_chain() {
        // A:0xH0000_B:0xH0001_0xH0002=0  → mem[0]-mem[1]+mem[2]==0
        // Set mem[0]=5, mem[1]=3, mem[2]=0 → 5-3+0=2 ≠ 0
        // Set mem[0]=5, mem[1]=5, mem[2]=0 → 5-5+0=0 ✓
        let trigger = run_frames("A:0xH0000_B:0xH0001_0xH0002=0", 5, |frame, mem| {
            mem.write(0x0002, 0);
            if frame >= 2 {
                mem.write(0x0000, 5);
                mem.write(0x0001, 5);
            } else {
                mem.write(0x0000, 5);
                mem.write(0x0001, 3);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    // -----------------------------------------------------------------------
    // Test 7: AddHits shared pool
    // -----------------------------------------------------------------------

    #[test]
    fn add_hits_shared_pool() {
        // C:0xH0000=1_0xH0001=1(5)
        // Each frame that mem[0]=1 OR mem[1]=1 contributes a hit.
        // The target on the terminal condition is 5.
        // If both are always 1, we get 2 hits per frame toward the pool.
        // After 3 frames: pool has 6 hits (3 from each row), meets target 5.
        let ach = Achievement::parse("C:0xH0000=1_0xH0001=1(5)").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1);
        mem.write(0x0001, 1);

        let mut triggered_frame = None;
        for frame in 1..=10usize {
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                triggered_frame = Some(frame);
                break;
            }
        }
        // With both conditions true every frame:
        // Frame 1: cond[0].hit=1, pool=1 contributed to cond[1]. cond[1] own hits...
        // AddHits means hits from cond[0] accumulate separately and are added
        // to cond[1]'s pool. After 3 frames cond[0].hit_count=3, cond[1].hit_count
        // grows; together they reach 5.
        assert!(triggered_frame.is_some(), "should have triggered");
        assert!(triggered_frame.unwrap() <= 5, "should trigger within 5 frames");
    }

    // -----------------------------------------------------------------------
    // Test 8: AndNext — both conditions must be true simultaneously
    // -----------------------------------------------------------------------

    #[test]
    fn and_next_both_true() {
        // N:0xH0000=1_0xH0001=2
        let trigger = run_frames("N:0xH0000=1_0xH0001=2", 5, |frame, mem| {
            if frame == 3 {
                mem.write(0x0000, 1);
                mem.write(0x0001, 2);
            }
        });
        assert_eq!(trigger, Some(3));
    }

    #[test]
    fn and_next_only_first_true_no_trigger() {
        let trigger = run_frames("N:0xH0000=1_0xH0001=2", 5, |_frame, mem| {
            mem.write(0x0000, 1);
            mem.write(0x0001, 0); // second fails
        });
        assert_eq!(trigger, None);
    }

    #[test]
    fn and_next_only_second_true_no_trigger() {
        let trigger = run_frames("N:0xH0000=1_0xH0001=2", 5, |_frame, mem| {
            mem.write(0x0000, 0); // first fails
            mem.write(0x0001, 2);
        });
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 9: OrNext — either condition being true is enough
    // -----------------------------------------------------------------------

    #[test]
    fn or_next_first_true() {
        // O:0xH0000=1_0xH0001=2  → trigger when mem[0]=1 OR mem[1]=2
        let trigger = run_frames("O:0xH0000=1_0xH0001=2", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 1); // only first true
                mem.write(0x0001, 0);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn or_next_second_true() {
        let trigger = run_frames("O:0xH0000=1_0xH0001=2", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 0);
                mem.write(0x0001, 2); // only second true
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn or_next_neither_true() {
        let trigger = run_frames("O:0xH0000=1_0xH0001=2", 5, |_frame, mem| {
            mem.write(0x0000, 0);
            mem.write(0x0001, 0);
        });
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 10: Measured — hit count progress exposed
    // -----------------------------------------------------------------------

    #[test]
    fn measured_hit_count_progress() {
        let ach = Achievement::parse("M:0xH0000=3(10)").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 3);

        // Run 5 frames.
        for _ in 0..5 {
            eval_achievement(&ach, &mut state, mem.peek_fn());
        }
        assert_eq!(state.core.measured, 5);
        assert_eq!(state.core.measured_target, 10);
        // Not triggered yet.
        assert!(!state.triggered);
    }

    #[test]
    fn measured_triggers_at_target() {
        let trigger = run_frames("M:0xH0000=3(5)", 10, |_frame, mem| {
            mem.write(0x0000, 3);
        });
        assert_eq!(trigger, Some(5));
    }

    // -----------------------------------------------------------------------
    // Test 11: MeasuredIf gates display (basic: condset still triggers)
    // -----------------------------------------------------------------------

    #[test]
    fn measured_if_does_not_block_trigger() {
        // Q:0xH0001=0_0xH0000=1 — MeasuredIf false but standard still triggers.
        let trigger = run_frames("Q:0xH0001=0_0xH0000=1", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 1);
                mem.write(0x0001, 1); // MeasuredIf false (!=0)
            }
        });
        // Standard condition at idx 1 passes; MeasuredIf does not block it.
        assert_eq!(trigger, Some(2));
    }

    // -----------------------------------------------------------------------
    // Test 12: Trigger flag — achievement stays primed until Trigger passes
    // -----------------------------------------------------------------------

    #[test]
    fn trigger_flag_stays_primed() {
        // 0xH0000=1_T:0xH0001=1 — core needs mem[0]=1 and Trigger mem[1]=1.
        let ach = Achievement::parse("0xH0000=1_T:0xH0001=1").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1); // standard always passes

        let mut frames_fired = vec![];
        for frame in 1..=5usize {
            if frame == 3 {
                mem.write(0x0001, 1); // Trigger passes on frame 3
            } else {
                mem.write(0x0001, 0);
            }
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                frames_fired.push(frame);
            }
        }
        assert_eq!(frames_fired, vec![3]);
    }

    // -----------------------------------------------------------------------
    // Test 13: Delta — triggers on increment (current > previous)
    // -----------------------------------------------------------------------

    #[test]
    fn delta_triggers_on_increment() {
        // `0xH0000>d0xH0000` — current byte > its delta (just increased)
        let ach = Achievement::parse("0xH0000>d0xH0000").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);

        // Frame 1: value=0, delta=0 (had_sample=false → delta=0). current==delta, no trigger.
        mem.write(0x0000, 0);
        let t1 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(!t1);

        // Frame 2: value still 0, delta becomes 0. Still no trigger.
        let t2 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(!t2);

        // Frame 3: set value=5, delta is still 0 from last frame. 5 > 0 → trigger.
        mem.write(0x0000, 5);
        let t3 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(t3);
    }

    // -----------------------------------------------------------------------
    // Test 14: Prior value
    // -----------------------------------------------------------------------

    #[test]
    fn prior_value_tracks_before_change() {
        // `0xH0000=p0xH0000` triggers when current == prior
        // (i.e., value hasn't changed since its *last* change — true after
        // a stable frame following a change)
        let ach = Achievement::parse("0xH0000=p0xH0000").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);

        // Frame 1: value=0, prior=0, last=0. 0==0 → triggers? Yes.
        mem.write(0x0000, 0);
        let t1 = eval_achievement(&ach, &mut state, mem.peek_fn());
        // First frame: had_sample was false so prior=0, current=0 → equal, trigger.
        assert!(t1);
    }

    #[test]
    fn prior_tracks_last_change() {
        // Verify prior_value is updated correctly across frames.
        // Use `p0xH0000` as the LHS (prior of address 0) compared against a constant.
        // When value first changes to 10, prior is still 5 → prior (5) != 10 is tested
        // by checking prior != current in a separate way.
        //
        // Simpler scenario: `p0xH0000=5` — triggers when the prior value of mem[0] is 5.
        // Frame 1: value=5, prior seeded=5. prior==5 → true. (but achievement latches)
        // Restart with fresh state; set value=0 first, then change to 5.
        let ach = Achievement::parse("p0xH0000=5").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);

        // Frame 1: value=0. Prior is seeded to 0 (first frame). prior(0)!=5 → no trigger.
        mem.write(0x0000, 0);
        let t1 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(!t1, "prior=0, should not trigger");

        // Frame 2: value=5. Prior is still 0 (last change: 0→5 sets prior=0).
        // prior(0) != 5 → no trigger.
        mem.write(0x0000, 5);
        let t2 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(!t2, "prior=0 (just changed from 0 to 5), should not trigger");

        // Frame 3: value still 5. Prior is 0 (changed last frame 0→5).
        // prior(0) != 5 → no trigger.
        let t3 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(!t3, "prior=0 still (value stable at 5), should not trigger");

        // Frame 4: change to 99. Prior = 5 (last change was 5). prior(5)==5 → trigger!
        mem.write(0x0000, 99);
        let t4 = eval_achievement(&ach, &mut state, mem.peek_fn());
        assert!(t4, "prior=5 now (changed from 5 to 99), should trigger");
    }

    // -----------------------------------------------------------------------
    // Test 15: Multiple condsets (core + two alts)
    // -----------------------------------------------------------------------

    #[test]
    fn multi_condset_core_plus_one_alt() {
        // Core: mem[0]=1.  Alt1: mem[1]=1.
        // Triggers only when both pass.
        let trigger = run_frames("0xH0000=1S0xH0001=1", 10, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 1);
            }
            if frame == 4 {
                mem.write(0x0001, 1);
            }
        });
        assert_eq!(trigger, Some(4));
    }

    #[test]
    fn multi_condset_core_passes_neither_alt() {
        // Two alts, neither passes → no trigger.
        let trigger = run_frames("0xH0000=1S0xH0001=99S0xH0002=99", 10, |_frame, mem| {
            mem.write(0x0000, 1);
            // alts 0x0001 and 0x0002 never reach 99
        });
        assert_eq!(trigger, None);
    }

    #[test]
    fn multi_condset_core_passes_second_alt() {
        let trigger = run_frames("0xH0000=1S0xH0001=99S0xH0002=7", 5, |frame, mem| {
            mem.write(0x0000, 1);
            if frame == 3 {
                mem.write(0x0002, 7); // second alt passes
            }
        });
        assert_eq!(trigger, Some(3));
    }

    // -----------------------------------------------------------------------
    // Test 16: AddAddress indirect read
    // -----------------------------------------------------------------------

    #[test]
    fn add_address_indirect_read() {
        // I:0xH0000_0xH0010=5
        // Address of the second condition = 0x10 + mem[0x0000].
        // Set mem[0x0000]=5 → effective address = 0x15.
        // Set mem[0x15]=5 → should trigger.
        let trigger = run_frames("I:0xH0000_0xH0010=5", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 5); // offset = 5
                mem.write(0x0015, 5); // mem[0x10 + 5] = 5
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn add_address_indirect_read_wrong_target() {
        // Same setup but the wrong address has the value — no trigger.
        let trigger = run_frames("I:0xH0000_0xH0010=5", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 5);
                mem.write(0x0010, 5); // base address, not offset address
            }
        });
        // With offset=5, we read mem[0x15] which is 0, not 5.
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 17: ResetNextIf resets only the following condition's hit count
    // -----------------------------------------------------------------------

    #[test]
    fn reset_next_if_resets_following_condition() {
        // Z:0xH0001=1_0xH0000=1(3)
        // mem[0]=1 always; Z fires on frame 3.
        // After Z on frame 3, cond[1]'s hits reset to 0.
        let ach = Achievement::parse("Z:0xH0001=1_0xH0000=1(3)").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1); // always true

        let mut triggered_frame = None;
        for frame in 1..=10usize {
            if frame == 3 {
                mem.write(0x0001, 1); // ResetNextIf fires
            } else {
                mem.write(0x0001, 0);
            }
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                triggered_frame = Some(frame);
                break;
            }
        }
        // Frame 1: hit=1, frame 2: hit=2
        // Frame 3: Z fires → hit_count reset to 0 then condition eval → hit=1
        // Frame 4: hit=2, frame 5: hit=3 → triggers.
        assert_eq!(triggered_frame, Some(5));
    }

    // -----------------------------------------------------------------------
    // Test 18: Greater / Less / NotEqual operators
    // -----------------------------------------------------------------------

    #[test]
    fn operator_greater_than() {
        let trigger = run_frames("0xH0000>10", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 11);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn operator_less_than() {
        let trigger = run_frames("0xH0000<10", 5, |_frame, mem| {
            mem.write(0x0000, 5);
        });
        assert_eq!(trigger, Some(1));
    }

    #[test]
    fn operator_not_equal() {
        let trigger = run_frames("0xH0000!=0", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 1);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    // -----------------------------------------------------------------------
    // Test 19: Achievement does not re-trigger after first trigger
    // -----------------------------------------------------------------------

    #[test]
    fn no_retrigger_after_first() {
        let ach = Achievement::parse("0xH0000=1").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1);

        let mut trigger_count = 0u32;
        for _ in 0..5 {
            if eval_achievement(&ach, &mut state, mem.peek_fn()) {
                trigger_count += 1;
            }
        }
        assert_eq!(trigger_count, 1);
    }

    // -----------------------------------------------------------------------
    // Test 20: Word memory read (16-bit LE)
    // -----------------------------------------------------------------------

    #[test]
    fn word_memory_read() {
        // 0x 0000 is a 16-bit LE read; rhs is h100 (hex immediate 0x100 = 256)
        let trigger = run_frames("0x 0000=h100", 5, |frame, mem| {
            if frame == 2 {
                mem.write_u16_le(0x0000, 0x100);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    // -----------------------------------------------------------------------
    // Test 21: AddSource with multiply modifier
    // -----------------------------------------------------------------------

    #[test]
    fn add_source_with_multiply() {
        // A:0xH0000*2_0xH0001=10 → mem[0]*2 + mem[1] == 10
        // Set mem[0]=3, mem[1]=4 → 3*2+4=10 ✓
        let trigger = run_frames("A:0xH0000*2_0xH0001=10", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 3);
                mem.write(0x0001, 4);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    // -----------------------------------------------------------------------
    // Test 22: Core fails → no trigger even if alts pass
    // -----------------------------------------------------------------------

    #[test]
    fn no_trigger_when_core_fails() {
        // Core: mem[0]=99 (never true). Alt: mem[1]=1.
        let trigger = run_frames("0xH0000=99S0xH0001=1", 5, |_frame, mem| {
            mem.write(0x0001, 1);
        });
        assert_eq!(trigger, None);
    }

    // -----------------------------------------------------------------------
    // Test 23: ResetIf in alt condset also resets hits globally
    // -----------------------------------------------------------------------

    #[test]
    fn resetif_in_alt_resets_core_hits() {
        // Core: 0xH0000=1(3). Alt: R:0xH0001=1 (reset when mem[1]=1)
        let ach = Achievement::parse("0xH0000=1(3)SR:0xH0001=1").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);
        mem.write(0x0000, 1);

        // Run 2 frames normally (2 hits).
        eval_achievement(&ach, &mut state, mem.peek_fn());
        eval_achievement(&ach, &mut state, mem.peek_fn());
        assert_eq!(state.core.cond_states[0].hit_count, 2);

        // Frame 3: fire ResetIf in alt.
        mem.write(0x0001, 1);
        eval_achievement(&ach, &mut state, mem.peek_fn());
        // Reset should have zeroed core hits.
        assert_eq!(state.core.cond_states[0].hit_count, 0);
    }

    // -----------------------------------------------------------------------
    // Test 24: Greater-or-equal and less-or-equal
    // -----------------------------------------------------------------------

    #[test]
    fn op_greater_or_equal() {
        let trigger = run_frames("0xH0000>=10", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 10);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn op_less_or_equal() {
        let trigger = run_frames("0xH0000<=10", 5, |_frame, mem| {
            mem.write(0x0000, 10);
        });
        assert_eq!(trigger, Some(1));
    }

    // -----------------------------------------------------------------------
    // Test 25: Nibble reads
    // -----------------------------------------------------------------------

    #[test]
    fn low_nibble_read() {
        // 0xL0000=3 → lower nibble of mem[0] == 3
        let trigger = run_frames("0xL0000=3", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 0x53); // low nibble = 3
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn high_nibble_read() {
        // 0xU0000=5 → upper nibble of mem[0] == 5
        let trigger = run_frames("0xU0000=5", 5, |frame, mem| {
            if frame == 2 {
                mem.write(0x0000, 0x53); // high nibble = 5
            }
        });
        assert_eq!(trigger, Some(2));
    }
}
