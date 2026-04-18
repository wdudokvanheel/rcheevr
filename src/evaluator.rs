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
    /// Last computed value from eval_value (returned when paused).
    pub last_val_result: i64,
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
    /// WAITING state: starts true for a fresh achievement. While waiting, the
    /// evaluator processes conditions and accumulates hit counts normally, but
    /// will NOT trigger. The first frame where the achievement would NOT
    /// trigger transitions from WAITING to ACTIVE (`waiting = false`). This
    /// matches rcheevos' anti-auto-trigger behaviour — achievements that are
    /// trivially true at boot cannot fire until they've been false at least
    /// once.
    pub waiting: bool,
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
        Self { core, alts, triggered: false, waiting_for_trigger_flag: false, waiting: true }
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
    // zero all hit counts across the whole achievement and prevent
    // triggering this frame (matches rcheevos: was_reset takes priority
    // over ret). In WAITING state, stay WAITING.
    // ------------------------------------------------------------------
    let any_reset = state.core.did_reset || state.alts.iter().any(|s| s.did_reset);
    if any_reset {
        reset_all_hits(state);
        return false;
    }

    // ------------------------------------------------------------------
    // Determine whether the achievement should trigger this frame.
    // ------------------------------------------------------------------

    let core_ok = state.core.triggered;

    let alts_ok = if a.alts.is_empty() {
        true
    } else {
        state.alts.iter().any(|s| s.triggered)
    };

    let would_trigger = core_ok && alts_ok;

    if state.waiting {
        if would_trigger {
            // Conditions are trivially satisfied — stay WAITING and reset
            // all hit counts (matches rcheevos: rc_reset_trigger called in
            // WAITING state when ret=true). This prevents hit inflation
            // while the achievement waits for conditions to become false.
            reset_all_hits(state);
        } else {
            // First frame where the trigger is NOT satisfied — arm it.
            // Hit counts are preserved (matching rcheevos).
            state.waiting = false;
        }
        return false;
    }

    if would_trigger {
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

    // Running sum for AddSource / SubSource chains (typed to support float promotion).
    let mut add_accum = Accum::Int(0);
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
    let mut new_paused = false;
    for (i, cond) in condset.conditions.iter().enumerate() {
        if cond.flag != ConditionFlag::PauseIf {
            continue;
        }
        let cs = &mut state.cond_states[i];
        // Always peek the raw current memory so delta/prior state stays accurate.
        let raw_current = peek_current(&cond.lhs, indirect_offset, peek);
        let prev_lhs = raw_current.map_or(0, |curr| update_delta(cs, curr));
        let lhs_val = resolve_comparison_lhs(&cond.lhs, cs, prev_lhs, indirect_offset, TypedValue::Unsigned(0), peek);
        let rhs_val = read_operand_static(&cond.rhs, peek);
        let passes = lhs_val.typed_compare(cond.op, rhs_val);
        if passes {
            if cond.required_hits == 0 {
                new_paused = true;
            } else {
                // PauseIf with hit target: pause once hit count reaches target.
                // Hit count increments unconditionally when condition is true,
                // matching rcheevos (rc_condset_evaluate_condition increments
                // current_hits regardless of pause state). Stops incrementing
                // once target is met (line 450-452 in condset.c).
                if cs.hit_count < cond.required_hits {
                    cs.hit_count += 1;
                }
                if cs.hit_count >= cond.required_hits {
                    new_paused = true;
                }
            }
        } else if cond.required_hits == 0 {
            // PauseIf without hit target: reset hit count when condition is
            // false (matches rcheevos condset.c:522-524).
            cs.hit_count = 0;
        } else if cs.hit_count >= cond.required_hits {
            // In rcheevos, once PauseIf's hit count reaches the target it
            // stays paused even when the condition is no longer true. Only
            // a ResetIf can clear the hit count and unpause.
            new_paused = true;
        }
    }
    state.is_paused = new_paused;
    state.did_reset = false;

    if state.is_paused {
        // When paused, do not advance any hit counts (including ResetIf).
        // However, we MUST still update delta/prior for all non-PauseIf
        // conditions so they stay current. In rcheevos, memref values are
        // updated globally (rc_update_trigger_memrefs) before condset
        // evaluation, so all delta/prior state remains accurate regardless
        // of pause. Our per-condition delta tracking needs this explicit
        // update to avoid stale deltas after unpause.
        //
        // PauseIf conditions were already updated in Phase 1.
        for (i, cond) in condset.conditions.iter().enumerate() {
            if cond.flag == ConditionFlag::PauseIf {
                continue;
            }
            let cs = &mut state.cond_states[i];
            // Peek with indirect_offset=0: correct for non-AddAddress
            // conditions. AddAddress-dependent addresses will peek the
            // base address, which is close enough (AddAddress + delta is
            // extremely rare in practice and rcheevos' modified memrefs
            // handle it differently anyway).
            if let Some(curr) = peek_current(&cond.lhs, 0, peek) {
                update_delta(cs, curr);
            }
        }
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
                let lhs_val = resolve_comparison_lhs_static(&cond.lhs, cs, indirect_offset, TypedValue::Unsigned(0), peek);
                let rhs_val = read_operand_static(&cond.rhs, peek);
                if lhs_val.typed_compare(cond.op, rhs_val) {
                    // In rcheevos, reset_next passes through modifier
                    // conditions (AddSource, SubSource, AddAddress, Remember)
                    // and is consumed by the next evaluable condition.
                    let mut j = i + 1;
                    while j < n {
                        match condset.conditions[j].flag {
                            ConditionFlag::AddSource
                            | ConditionFlag::SubSource
                            | ConditionFlag::AddAddress
                            | ConditionFlag::Remember => { j += 1; }
                            _ => break,
                        }
                    }
                    if j < n {
                        reset_next_set[j] = true;
                    }
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
    let mut remember_val = TypedValue::Unsigned(0); // Recall/Remember register (typed)

    let n = condset.conditions.len();
    let mut i = 0;
    while i < n {
        let cond = &condset.conditions[i];
        let cs = &mut state.cond_states[i];

        // Apply ResetNextIf: zero this condition's hit count AND force the
        // condition false for this frame (matching rcheevos: cond_valid = 0).
        let forced_false = reset_next_set[i];
        if forced_false {
            cs.hit_count = 0;
        }

        // Step 1: Peek the raw current memory value for delta/prior tracking.
        let raw_current = peek_current(&cond.lhs, indirect_offset, peek);
        // Step 2: Update delta/prior state; returns the previous last_value
        // (which is the correct "delta" comparison value for this frame).
        let prev_lhs = raw_current.map_or(0, |curr| update_delta(cs, curr));
        // Step 3: Resolve the actual comparison value from the operand kind.
        let raw_lhs = resolve_comparison_lhs(&cond.lhs, cs, prev_lhs, indirect_offset, remember_val, peek);
        // RHS uses the same condition state so that Delta/Prior on the RHS
        // (e.g. `0xH0000>d0xH0000`) returns the correct previous-frame value.
        let raw_rhs = read_operand_rhs(&cond.rhs, cs, prev_lhs, remember_val, peek);

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
                let val = apply_modifier_typed(raw_lhs, cond.op, raw_rhs);
                add_accum = add_accum.add(val);
                indirect_offset = 0;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // SubSource: subtract from add_accum.
            // ------------------------------------------------------------------
            ConditionFlag::SubSource => {
                let val = apply_modifier_typed(raw_lhs, cond.op, raw_rhs);
                add_accum = add_accum.sub(val);
                indirect_offset = 0;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AddAddress: accumulate address offset.
            // ------------------------------------------------------------------
            ConditionFlag::AddAddress => {
                let val = apply_modifier_typed(raw_lhs, cond.op, raw_rhs);
                indirect_offset = indirect_offset.wrapping_add(val.to_u32());
                // Do NOT reset indirect_offset here — it carries forward.
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Remember: store the effective LHS (including AddSource chain)
            // into the remember register. Read back via {recall} operands.
            // ------------------------------------------------------------------
            ConditionFlag::Remember => {
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;
                remember_val = effective_lhs;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // AndNext: fold comparison result into the and_next gate.
            // ------------------------------------------------------------------
            ConditionFlag::AndNext => {
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;
                let passes = effective_lhs.typed_compare(cond.op, raw_rhs);
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
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;
                let passes = effective_lhs.typed_compare(cond.op, raw_rhs);
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
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
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
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);
                if passes {
                    cs.hit_count += 1;
                }
                shared_hits_delta -= cs.hit_count as i64;
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // ResetIf: zero all hit counts when condition fires.
            //
            // Matches rcheevos: ResetIf respects required_hits (hit target).
            // The condition is gated by AndNext/OrNext chains. Once the hit
            // target is met, the ResetIf stays active even if the condition
            // becomes false — only a global reset clears the hit count.
            // ------------------------------------------------------------------
            ConditionFlag::ResetIf => {
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                let fires = if cond.required_hits == 0 {
                    // No hit target: fire whenever condition passes.
                    if passes {
                        cs.hit_count += 1;
                    }
                    passes
                } else {
                    // Hit target: accumulate hits, fire once target met.
                    if passes && cs.hit_count < cond.required_hits {
                        cs.hit_count += 1;
                    }
                    // Once met, stays active until a global reset clears hits
                    // (matches rcheevos: condition false + hits==target → still fires).
                    cs.hit_count >= cond.required_hits
                };

                if fires {
                    state.did_reset = true;
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // Measured: count hits (or raw value) and expose as progress.
            // ------------------------------------------------------------------
            ConditionFlag::Measured => {
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
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
                    // Value mode: like Standard — condition must be true right now.
                    // rcheevos calls rc_condset_evaluate_standard for this case.
                    if !passes {
                        all_satisfied = false;
                    }
                }
                i += 1;
                continue;
            }

            // ------------------------------------------------------------------
            // MeasuredIf: gates both condset satisfaction AND measured display.
            //
            // Matches rcheevos: is_true &= cond_valid, is_primed &= cond_valid,
            // can_measure &= cond_valid. MeasuredIf participates in hit-count
            // tracking just like Standard. When false, it prevents the condset
            // from being satisfied and zeroes the measured display value.
            // ------------------------------------------------------------------
            ConditionFlag::MeasuredIf => {
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                if passes {
                    cs.hit_count += 1;
                }

                // Satisfaction check (like Standard).
                let satisfied = if cond.required_hits == 0 {
                    passes
                } else {
                    cs.hit_count >= cond.required_hits
                };

                if !satisfied {
                    all_satisfied = false;
                    // Gate measured display: zero the measured value when MeasuredIf fails.
                    state.measured = 0;
                }

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
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
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
                let effective_lhs = add_accum.apply_to(raw_lhs);
                add_accum = Accum::Int(0);
                indirect_offset = 0;

                let cmp = effective_lhs.typed_compare(cond.op, raw_rhs);
                let passes = if forced_false { false } else { apply_gate(cmp, and_next, in_or_next, or_next_active) };
                reset_and_or(&mut and_next, &mut in_or_next, &mut or_next_active);

                // Always increment terminal's own hit count when condition
                // passes — this tracks independently of the AddHits/SubHits
                // pool (matching rcheevos current_hits semantics).
                if passes {
                    cs.hit_count += 1;
                }

                // Compute effective hits: own count + AddHits/SubHits pool.
                let effective_hits = if shared_hits_delta != 0 {
                    let total = cs.hit_count as i64 + shared_hits_delta;
                    shared_hits_delta = 0; // pool consumed
                    total.max(0) as u32
                } else {
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
            let raw = peek(addr, base_read_size(*size));
            Some(transform_memref_value(raw, *size))
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
    remember_val: TypedValue,
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
                    if is_float { TypedValue::Float(f32::from_bits(prev_lhs)) } else { TypedValue::Unsigned(prev_lhs) }
                }
                OperandKind::Prior => {
                    if is_float { TypedValue::Float(f32::from_bits(cs.prior_value)) } else { TypedValue::Unsigned(cs.prior_value) }
                }
                OperandKind::Bcd => {
                    let raw = peek(addr, base_read_size(*size));
                    let val = transform_memref_value(raw, *size);
                    // rcheevos skips BCD for float types
                    if is_float { TypedValue::Float(f32::from_bits(val)) } else { TypedValue::Unsigned(bcd_decode(val)) }
                }
                OperandKind::Inverted => {
                    let raw = peek(addr, base_read_size(*size));
                    let val = transform_memref_value(raw, *size);
                    // rcheevos skips Invert for float types
                    if is_float { TypedValue::Float(f32::from_bits(val)) } else { TypedValue::Unsigned(!val & size_mask(*size)) }
                }
                OperandKind::Recall => remember_val,
            }
        }
        Operand::ConstInt(v) => TypedValue::Unsigned(*v),
        Operand::ConstFloat(f) => TypedValue::Float(*f),
    }
}

/// Like `resolve_comparison_lhs` but without delta tracking (for pre-scans that
/// only need the current comparison value without advancing state).
fn resolve_comparison_lhs_static<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    indirect_offset: u32,
    remember_val: TypedValue,
    peek: &mut F,
) -> TypedValue {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = address.wrapping_add(indirect_offset);
            let is_float = is_float_size(*size);
            match kind {
                OperandKind::Address | OperandKind::Bcd | OperandKind::Inverted => {
                    let raw = peek(addr, base_read_size(*size));
                    wrap_value(transform_memref_value(raw, *size), *size)
                }
                OperandKind::Delta => {
                    let v = if cs.had_sample { cs.last_value } else { 0 };
                    if is_float { TypedValue::Float(f32::from_bits(v)) } else { TypedValue::Unsigned(v) }
                }
                OperandKind::Prior => {
                    if is_float { TypedValue::Float(f32::from_bits(cs.prior_value)) } else { TypedValue::Unsigned(cs.prior_value) }
                }
                OperandKind::Recall => remember_val,
            }
        }
        Operand::ConstInt(v) => TypedValue::Unsigned(*v),
        Operand::ConstFloat(f) => TypedValue::Float(*f),
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
    remember_val: TypedValue,
    peek: &mut F,
) -> TypedValue {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = *address;
            let is_float = is_float_size(*size);
            match kind {
                OperandKind::Address => {
                    let raw = peek(addr, base_read_size(*size));
                    wrap_value(transform_memref_value(raw, *size), *size)
                }
                OperandKind::Delta => {
                    if is_float { TypedValue::Float(f32::from_bits(prev_lhs)) } else { TypedValue::Unsigned(prev_lhs) }
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

/// Read an operand when we have no associated condition state.
/// For memory operands this always reads the current value (no delta/prior).
/// Used for PauseIf pre-scan and static RHS without delta context.
fn read_operand_static<F: FnMut(u32, MemSize) -> u32>(op: &Operand, peek: &mut F) -> TypedValue {
    match op {
        Operand::Memory { kind: _, size, address } => {
            let raw = peek(*address, base_read_size(*size));
            wrap_value(transform_memref_value(raw, *size), *size)
        }
        Operand::ConstInt(v) => TypedValue::Unsigned(*v),
        Operand::ConstFloat(f) => TypedValue::Float(*f),
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
// Size transformation: raw peek → extracted value
//
// Matches rcheevos' rc_transform_memref_value.  The evaluator calls peek with
// base_read_size (byte-granularity) then applies transform_memref_value to
// extract sub-byte quantities (nibbles, bits, big-endian byte-swap).
// ---------------------------------------------------------------------------

/// Map a MemSize to the byte-granularity size the peek should actually read.
/// Sub-byte sizes (nibbles, bits) map to Byte; big-endian variants map to
/// their little-endian byte count equivalent.
pub(crate) fn base_read_size(size: MemSize) -> MemSize {
    match size {
        MemSize::LowNibble
        | MemSize::HighNibble
        | MemSize::BitCount
        | MemSize::Bit0
        | MemSize::Bit1
        | MemSize::Bit2
        | MemSize::Bit3
        | MemSize::Bit4
        | MemSize::Bit5
        | MemSize::Bit6
        | MemSize::Bit7 => MemSize::Byte,
        MemSize::BigEndianWord => MemSize::Word,
        MemSize::BigEndianThreeBytes => MemSize::ThreeBytes,
        MemSize::BigEndianDWord | MemSize::FloatBigEndian | MemSize::Double32BigEndian => {
            MemSize::DWord
        }
        other => other,
    }
}

/// Extract the meaningful value from a raw peek result according to MemSize.
pub(crate) fn transform_memref_value(raw: u32, size: MemSize) -> u32 {
    match size {
        // Pass-through: no sub-byte extraction.
        MemSize::Byte | MemSize::Word | MemSize::ThreeBytes | MemSize::DWord => raw,
        // IEEE 754 float (LE): raw is already the f32 bit pattern.
        MemSize::Float => raw,
        // MBF/Double32 floats: decode to f32 and store as bits.
        MemSize::MbfFloat => decode_mbf32(raw).to_bits(),
        MemSize::MbfFloatLittleEndian => decode_mbf32_le(raw).to_bits(),
        MemSize::Double32 => decode_double32(raw).to_bits(),
        // Nibbles
        MemSize::LowNibble => raw & 0x0F,
        MemSize::HighNibble => (raw >> 4) & 0x0F,
        // Individual bits
        MemSize::Bit0 => raw & 1,
        MemSize::Bit1 => (raw >> 1) & 1,
        MemSize::Bit2 => (raw >> 2) & 1,
        MemSize::Bit3 => (raw >> 3) & 1,
        MemSize::Bit4 => (raw >> 4) & 1,
        MemSize::Bit5 => (raw >> 5) & 1,
        MemSize::Bit6 => (raw >> 6) & 1,
        MemSize::Bit7 => (raw >> 7) & 1,
        // Popcount of byte
        MemSize::BitCount => (raw as u8).count_ones(),
        // Big-endian integer: byte-swap
        MemSize::BigEndianWord => ((raw >> 8) & 0xFF) | ((raw & 0xFF) << 8),
        MemSize::BigEndianThreeBytes => {
            ((raw >> 16) & 0xFF) | (raw & 0xFF00) | ((raw & 0xFF) << 16)
        }
        MemSize::BigEndianDWord => raw.swap_bytes(),
        // Big-endian float: byte-swap gives IEEE 754 LE bits.
        MemSize::FloatBigEndian => raw.swap_bytes(),
        // Big-endian double32: byte-swap to LE, then decode.
        MemSize::Double32BigEndian => decode_double32(raw.swap_bytes()).to_bits(),
    }
}

// ---------------------------------------------------------------------------
// Size mask (for Inverted operand)
// ---------------------------------------------------------------------------

pub(crate) fn size_mask(size: MemSize) -> u32 {
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

pub(crate) fn bcd_decode(raw: u32) -> u32 {
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
// Typed value support (float-aware comparison and accumulation)
// ---------------------------------------------------------------------------

/// A value tagged with its type. Used for float-aware comparison and
/// accumulator arithmetic, matching rcheevos' `rc_typed_value_t`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TypedValue {
    Unsigned(u32),
    Float(f32),
}

impl TypedValue {
    pub(crate) fn to_u32(self) -> u32 {
        match self {
            TypedValue::Unsigned(v) => v,
            TypedValue::Float(f) => f as u32,
        }
    }

    pub(crate) fn to_f32(self) -> f32 {
        match self {
            TypedValue::Unsigned(v) => v as f32,
            TypedValue::Float(f) => f,
        }
    }

    pub(crate) fn is_float(self) -> bool {
        matches!(self, TypedValue::Float(_))
    }

    pub(crate) fn typed_compare(self, op: CompareOp, other: TypedValue) -> bool {
        if self.is_float() || other.is_float() {
            compare_float(self.to_f32(), op, other.to_f32())
        } else {
            compare(self.to_u32(), op, other.to_u32())
        }
    }
}

/// Returns true if the MemSize represents a float-decoded value.
pub(crate) fn is_float_size(size: MemSize) -> bool {
    matches!(
        size,
        MemSize::Float
            | MemSize::FloatBigEndian
            | MemSize::MbfFloat
            | MemSize::MbfFloatLittleEndian
            | MemSize::Double32
            | MemSize::Double32BigEndian
    )
}

/// Wrap a raw u32 (from transform_memref_value) as a TypedValue.
pub(crate) fn wrap_value(val: u32, size: MemSize) -> TypedValue {
    if is_float_size(size) {
        TypedValue::Float(f32::from_bits(val))
    } else {
        TypedValue::Unsigned(val)
    }
}

/// Typed accumulator for AddSource/SubSource chains.
/// Starts as integer, promotes to float when a float value is added.
#[derive(Clone, Copy)]
enum Accum {
    Int(i64),
    Float(f64),
}

impl Accum {
    fn add(self, val: TypedValue) -> Self {
        match (self, val) {
            (Accum::Int(acc), TypedValue::Unsigned(v)) => Accum::Int(acc + v as i64),
            (Accum::Int(acc), TypedValue::Float(f)) => Accum::Float(acc as f64 + f as f64),
            (Accum::Float(acc), TypedValue::Unsigned(v)) => Accum::Float(acc + v as f64),
            (Accum::Float(acc), TypedValue::Float(f)) => Accum::Float(acc + f as f64),
        }
    }

    fn sub(self, val: TypedValue) -> Self {
        match (self, val) {
            (Accum::Int(acc), TypedValue::Unsigned(v)) => Accum::Int(acc - v as i64),
            (Accum::Int(acc), TypedValue::Float(f)) => Accum::Float(acc as f64 - f as f64),
            (Accum::Float(acc), TypedValue::Unsigned(v)) => Accum::Float(acc - v as f64),
            (Accum::Float(acc), TypedValue::Float(f)) => Accum::Float(acc - f as f64),
        }
    }

    /// Apply the accumulated offset to a raw LHS value.
    fn apply_to(self, raw_lhs: TypedValue) -> TypedValue {
        match self {
            Accum::Int(0) => raw_lhs,
            Accum::Int(acc) => match raw_lhs {
                TypedValue::Unsigned(v) => TypedValue::Unsigned((v as i64 + acc) as u32),
                TypedValue::Float(f) => TypedValue::Float((f as f64 + acc as f64) as f32),
            },
            Accum::Float(acc) => match raw_lhs {
                TypedValue::Unsigned(v) => TypedValue::Float((v as f64 + acc) as f32),
                TypedValue::Float(f) => TypedValue::Float((f as f64 + acc) as f32),
            },
        }
    }
}

/// Apply a modifier operation with typed values. Bitwise ops force unsigned.
fn apply_modifier_typed(lhs: TypedValue, op: CompareOp, rhs: TypedValue) -> TypedValue {
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
                    CompareOp::Divide => {
                        if b == 0.0 {
                            0.0
                        } else {
                            a / b
                        }
                    }
                    CompareOp::Modulo => {
                        if b == 0.0 {
                            0.0
                        } else {
                            a % b
                        }
                    }
                    _ => a,
                })
            } else {
                TypedValue::Unsigned(apply_modifier(lhs.to_u32(), op, rhs.to_u32()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Float comparison with epsilon tolerance (matches rcheevos)
// ---------------------------------------------------------------------------

/// Compare two f32 values with epsilon tolerance matching rcheevos'
/// `rc_typed_value_compare_floats`.
fn compare_float(lhs: f32, op: CompareOp, rhs: f32) -> bool {
    if lhs == rhs {
        // exactly equal
        return matches!(
            op,
            CompareOp::Equal | CompareOp::LessOrEqual | CompareOp::GreaterOrEqual
        );
    }

    let abs1 = lhs.abs();
    let abs2 = rhs.abs();
    let threshold = abs1.min(abs2) * f32::EPSILON;
    let diff = lhs - rhs;

    if diff.abs() <= threshold {
        // approximately equal
        return matches!(
            op,
            CompareOp::Equal | CompareOp::LessOrEqual | CompareOp::GreaterOrEqual
        );
    }

    if diff > threshold {
        // lhs > rhs
        matches!(
            op,
            CompareOp::NotEqual | CompareOp::Greater | CompareOp::GreaterOrEqual
        )
    } else {
        // lhs < rhs
        matches!(
            op,
            CompareOp::NotEqual | CompareOp::Less | CompareOp::LessOrEqual
        )
    }
}

// ---------------------------------------------------------------------------
// Float decoders (MBF32, Double32)
// ---------------------------------------------------------------------------

/// Build a float from 23-bit mantissa, exponent, and sign.
/// Matches rcheevos' `rc_build_float`.
fn build_float(mantissa_bits: u32, exponent: i32, sign: bool) -> f32 {
    let implied_bit: u32 = 1 << 23;
    let mantissa = mantissa_bits | implied_bit;
    let mut dbl = mantissa as f64 / implied_bit as f64;

    if exponent > 127 {
        if mantissa_bits == 0 {
            dbl = f64::INFINITY;
        } else {
            dbl = f64::NAN;
        }
    } else if exponent >= 0 {
        dbl *= (2.0f64).powi(exponent);
    } else {
        dbl /= (2.0f64).powi(-exponent);
    }

    if sign {
        dbl = -dbl;
    }
    dbl as f32
}

/// Decode the high 32 bits of an IEEE 754 double into f32.
fn decode_double32(raw: u32) -> f32 {
    let mantissa = (raw & 0x000FFFFF) << 3;
    let exponent = ((raw >> 20) & 0x7FF) as i32 - 1023;
    let sign = (raw & 0x80000000) != 0;
    build_float(mantissa, exponent, sign)
}

/// Decode a Microsoft Binary Format 32-bit float (big-endian in memory).
fn decode_mbf32(raw: u32) -> f32 {
    let mantissa = ((raw & 0xFF000000) >> 24) | ((raw & 0x00FF0000) >> 8) | ((raw & 0x00007F00) << 8);
    let exponent = (raw & 0xFF) as i32 - 129;
    let sign = (raw & 0x00008000) != 0;

    if mantissa == 0 && exponent == -129 {
        if sign {
            -0.0
        } else {
            0.0
        }
    } else {
        build_float(mantissa, exponent, sign)
    }
}

/// Decode a Microsoft Binary Format 32-bit float (little-endian).
fn decode_mbf32_le(raw: u32) -> f32 {
    let mantissa = raw & 0x007FFFFF;
    let exponent = (raw >> 24) as i32 - 129;
    let sign = (raw & 0x00800000) != 0;

    if mantissa == 0 && exponent == -129 {
        if sign {
            -0.0
        } else {
            0.0
        }
    } else {
        build_float(mantissa, exponent, sign)
    }
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

        /// Return raw little-endian bytes. The evaluator handles sub-byte
        /// extraction (nibbles, bits) via `transform_memref_value`.
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
    // Test 11: MeasuredIf gates satisfaction (matches rcheevos: is_true &= cond_valid)
    // -----------------------------------------------------------------------

    #[test]
    fn measured_if_blocks_trigger_when_false() {
        // Q:0xH0001=0_0xH0000=1 — MeasuredIf false blocks trigger.
        // Frame 1: mem[0]=0, mem[1]=0. MeasuredIf: 0=0 true. Standard: 0≠1 false.
        //          Not satisfied → exits WAITING.
        // Frame 2: mem[0]=1, mem[1]=1. MeasuredIf: 1≠0 false → blocks. No trigger.
        // Frame 3: mem[0]=1, mem[1]=0. MeasuredIf: 0=0 true. Standard: 1=1 true → triggers!
        let trigger = run_frames("Q:0xH0001=0_0xH0000=1", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 1);
            }
            if frame == 2 {
                mem.write(0x0001, 1); // MeasuredIf false → blocks
            } else {
                mem.write(0x0001, 0); // MeasuredIf true
            }
        });
        assert_eq!(trigger, Some(3));
    }

    #[test]
    fn measured_if_allows_trigger_when_true() {
        // Q:0xH0001=1_0xH0000=1 — MeasuredIf true allows trigger.
        let trigger = run_frames("Q:0xH0001=1_0xH0000=1", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 1);
                mem.write(0x0001, 1); // MeasuredIf true
            }
        });
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
    fn waiting_blocks_always_true_condition() {
        // `0xH0000=p0xH0000` — current == prior. True from frame 1 (both 0).
        // rcheevos WAITING state prevents triggering when the condition is
        // never false. This is the anti-auto-trigger behaviour.
        let ach = Achievement::parse("0xH0000=p0xH0000").unwrap();
        let mut state = AchievementState::for_achievement(&ach);
        let mut mem = Mem::new(0x10000);

        for _ in 0..20 {
            assert!(!eval_achievement(&ach, &mut state, mem.peek_fn()));
        }
        assert!(state.waiting, "should stay in WAITING when condition is always true");
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
        // Frame 3: Z fires → hit_count reset to 0, forced false → hit stays 0
        // Frame 4: hit=1, frame 5: hit=2, frame 6: hit=3 → triggers.
        assert_eq!(triggered_frame, Some(6));
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
        let trigger = run_frames("0xH0000<10", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 5); // 5 < 10 → true from frame 2
            } else {
                mem.write(0x0000, 20); // 20 < 10 → false → exits WAITING
            }
        });
        assert_eq!(trigger, Some(2));
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

        // Frame 1: condition false → exits WAITING.
        assert!(!eval_achievement(&ach, &mut state, mem.peek_fn()));

        // Frame 2+: condition true — should fire exactly once.
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
        let trigger = run_frames("0xH0000<=10", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 10); // 10 <= 10 → true from frame 2
            } else {
                mem.write(0x0000, 20); // 20 <= 10 → false → exits WAITING
            }
        });
        assert_eq!(trigger, Some(2));
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

    // -----------------------------------------------------------------------
    // Test 26: Remember/Recall
    // -----------------------------------------------------------------------

    #[test]
    fn remember_recall_basic() {
        // K:0xH0000_{recall}=5
        // Remember stores mem[0] (no operator/RHS — modifier row).
        // Then {recall}=5 checks if stored value == 5.
        // Frame 1: mem[0]=0 → remember 0, 0!=5 → false → exits WAITING
        // Frame 2: mem[0]=5 → remember 5, 5==5 → triggers
        let trigger = run_frames("K:0xH0000_{recall}=5", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 5);
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn remember_recall_with_add_source() {
        // A:0xH0000_K:0xH0001_{recall}=10
        // AddSource mem[0] + Remember mem[1] → recall = mem[0]+mem[1]
        // Check {recall}=10
        let trigger = run_frames("A:0xH0000_K:0xH0001_{recall}=10", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 3);
                mem.write(0x0001, 7); // 3+7=10
            }
        });
        assert_eq!(trigger, Some(2));
    }

    #[test]
    fn remember_recall_rhs() {
        // K:0xH0000_0xH0001={recall}
        // Remember stores mem[0], then check mem[1]=={recall} (stored value)
        // Frame 1: mem[0]=0, mem[1]=0 → remember 0, 0==0 → would trigger but WAITING
        //          Actually: 0==0 is true → stays WAITING
        // Frame 2: mem[0]=5, mem[1]=0 → remember 5, 0!=5 → exits WAITING
        // Frame 3: mem[0]=5, mem[1]=5 → remember 5, 5==5 → triggers
        let trigger = run_frames("K:0xH0000_0xH0001={recall}", 5, |frame, mem| {
            if frame >= 2 {
                mem.write(0x0000, 5);
            }
            if frame >= 3 {
                mem.write(0x0001, 5);
            }
        });
        assert_eq!(trigger, Some(3));
    }
}
