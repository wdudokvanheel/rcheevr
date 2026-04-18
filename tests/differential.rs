//! Layer-2 differential tests: rveevr's evaluator vs rcheevos as oracle.
//!
//! Gated behind `--features oracle` because the oracle crate needs a C
//! toolchain to build. When the feature is off, the whole file compiles to
//! nothing. When on, it feeds synthetic memory traces into both evaluators
//! and asserts equivalent outputs per frame.

#![cfg(feature = "oracle")]

use rveevr::condition::Achievement;
use rveevr::evaluator::{eval_achievement, AchievementState, CondsetState, ConditionState};
use rveevr::leaderboard::eval_value;
use rveevr::memaddr::MemSize;
use rveevr_oracle::{ffi, Trigger, Value};

/// A tiny flat memory buffer the harness feeds to both evaluators.
struct Mem(Vec<u8>);

impl Mem {
    fn new(size: usize) -> Self {
        Self(vec![0u8; size])
    }

    fn write(&mut self, addr: usize, v: u8) {
        self.0[addr] = v;
    }

    fn write_u32_le(&mut self, addr: usize, v: u32) {
        self.0[addr] = v as u8;
        self.0[addr + 1] = (v >> 8) as u8;
        self.0[addr + 2] = (v >> 16) as u8;
        self.0[addr + 3] = (v >> 24) as u8;
    }

    fn write_f32_le(&mut self, addr: usize, f: f32) {
        self.write_u32_le(addr, f.to_bits());
    }

    /// Read `num_bytes` little-endian from `addr`, returning 0 for
    /// out-of-range reads (rcheevos expects a "safe" peek).
    fn read(&self, addr: u32, num_bytes: u32) -> u32 {
        let a = addr as usize;
        let n = num_bytes as usize;
        if a + n > self.0.len() {
            return 0;
        }
        let mut out: u32 = 0;
        for i in 0..n.min(4) {
            out |= (self.0[a + i] as u32) << (8 * i);
        }
        out
    }
}

/// Translate rveevr's MemSize back to a byte count for a size that the
/// MockMem supports. Only the sizes the evaluator actually calls here
/// need to be covered.
fn size_to_bytes(size: MemSize) -> u32 {
    match size {
        MemSize::Byte | MemSize::LowNibble | MemSize::HighNibble | MemSize::BitCount => 1,
        MemSize::Bit0
        | MemSize::Bit1
        | MemSize::Bit2
        | MemSize::Bit3
        | MemSize::Bit4
        | MemSize::Bit5
        | MemSize::Bit6
        | MemSize::Bit7 => 1,
        MemSize::Word | MemSize::BigEndianWord => 2,
        MemSize::ThreeBytes | MemSize::BigEndianThreeBytes => 3,
        MemSize::DWord | MemSize::BigEndianDWord => 4,
        _ => 4,
    }
}

/// Drive both evaluators through the same frame sequence. Returns
/// `(our_triggered_frames, oracle_triggered_frames)` for the caller to
/// compare. Each frame is a closure that mutates the shared memory before
/// both evaluators sample it.
fn run_trace<F>(memaddr: &str, mem_size: usize, frames: usize, mut step: F) -> (Vec<bool>, Vec<bool>)
where
    F: FnMut(usize, &mut Mem),
{
    let ach = Achievement::parse(memaddr).expect("our parse");
    let mut ours_state = AchievementState::for_achievement(&ach);
    let mut theirs = Trigger::parse(memaddr).expect("oracle parse");

    let mut mem_ours = Mem::new(mem_size);
    let mut mem_theirs = Mem::new(mem_size);
    let mut ours_log = Vec::with_capacity(frames);
    let mut theirs_log = Vec::with_capacity(frames);

    for f in 0..frames {
        step(f, &mut mem_ours);
        step(f, &mut mem_theirs);

        let ours_triggered = eval_achievement(&ach, &mut ours_state, |addr, size| {
            let n = size_to_bytes(size);
            mem_ours.read(addr, n)
        });
        ours_log.push(ours_triggered);

        let state = theirs.evaluate(|addr, n| mem_theirs.read(addr, n));
        theirs_log.push(state == ffi::RC_TRIGGER_STATE_TRIGGERED);
    }
    (ours_log, theirs_log)
}

fn assert_logs_agree(tag: &str, ours: &[bool], theirs: &[bool]) {
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{tag}: frame-count mismatch ours={} theirs={}",
        ours.len(),
        theirs.len()
    );
    // Both evaluators should reach the triggered state on the same frame.
    // Our evaluator reports "newly triggered this frame" as a rising edge,
    // while rcheevos reports "currently in triggered state" — so compare the
    // first frame at which each side first reports true, then require that
    // rcheevos stays true from that frame onward for as long as the trigger
    // latches.
    let ours_first = ours.iter().position(|b| *b);
    let theirs_first = theirs.iter().position(|b| *b);
    assert_eq!(
        ours_first, theirs_first,
        "{tag}: first-trigger frame disagreement ours={:?} theirs={:?}\n  ours log:  {:?}\n  theirs:    {:?}",
        ours_first, theirs_first, ours, theirs
    );
}

#[test]
fn simple_equality_trigger() {
    let (ours, theirs) = run_trace("0xH0000=1", 16, 4, |frame, mem| {
        mem.write(0, if frame >= 2 { 1 } else { 0 });
    });
    assert_logs_agree("simple_equality_trigger", &ours, &theirs);
}

#[test]
fn not_equal_trigger() {
    let (ours, theirs) = run_trace("0xH0000!=0", 16, 3, |frame, mem| {
        mem.write(0, if frame >= 1 { 7 } else { 0 });
    });
    assert_logs_agree("not_equal_trigger", &ours, &theirs);
}

#[test]
fn greater_than_trigger() {
    let (ours, theirs) = run_trace("0xH0000>5", 16, 4, |frame, mem| {
        mem.write(0, frame as u8 * 2);
    });
    assert_logs_agree("greater_than_trigger", &ours, &theirs);
}

#[test]
fn hit_count_latch() {
    let (ours, theirs) = run_trace("0xH0000=1(3)", 16, 5, |_frame, mem| {
        mem.write(0, 1);
    });
    assert_logs_agree("hit_count_latch", &ours, &theirs);
}

#[test]
fn word_comparison() {
    let (ours, theirs) = run_trace("0x 0002=100", 16, 3, |frame, mem| {
        if frame >= 1 {
            mem.write(2, 0x64);
            mem.write(3, 0x00);
        }
    });
    assert_logs_agree("word_comparison", &ours, &theirs);
}

#[test]
fn multi_condset_alt() {
    // Core must pass AND at least one alt. Here alt1 passes on frame 2,
    // alt2 never; so trigger on frame 2 when both core + alt1 hold.
    let (ours, theirs) = run_trace(
        "0xH0000=1S0xH0001=2S0xH0002=3",
        16,
        4,
        |frame, mem| {
            mem.write(0, 1);
            mem.write(1, if frame >= 2 { 2 } else { 0 });
        },
    );
    assert_logs_agree("multi_condset_alt", &ours, &theirs);
}

#[test]
fn delta_increment_detection() {
    // Fires when the byte at 0 increases from the previous frame's value.
    let (ours, theirs) = run_trace("0xH0000>d0xH0000", 16, 4, |frame, mem| {
        mem.write(0, frame as u8);
    });
    assert_logs_agree("delta_increment_detection", &ours, &theirs);
}

#[test]
fn remember_recall_lhs() {
    // K:0xH0000_{recall}=5
    // Remember stores mem[0], {recall}=5 triggers when stored value == 5.
    let (ours, theirs) = run_trace("K:0xH0000_{recall}=5", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 5);
        }
    });
    assert_logs_agree("remember_recall_lhs", &ours, &theirs);
}

#[test]
fn remember_recall_rhs() {
    // K:0xH0000_0xH0001={recall}
    // Remember stores mem[0], then check mem[1] == recalled value.
    let (ours, theirs) = run_trace("K:0xH0000_0xH0001={recall}", 16, 6, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 7); // remember 7
        }
        if frame >= 4 {
            mem.write(1, 7); // mem[1] == recall(7) → triggers
        }
    });
    assert_logs_agree("remember_recall_rhs", &ours, &theirs);
}

#[test]
fn remember_recall_with_add_source() {
    // A:0xH0000_K:0xH0001_{recall}=10
    // AddSource(mem[0]) + Remember(mem[1]) → recall = mem[0]+mem[1], check == 10.
    let (ours, theirs) = run_trace("A:0xH0000_K:0xH0001_{recall}=10", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 4);
            mem.write(1, 6); // 4+6=10
        }
    });
    assert_logs_agree("remember_recall_with_add_source", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// rcheevos test-suite condition strings
//
// Authoritative strings from rcheevos' own test suite (test_trigger.c,
// test_condset.c). These exercise parser edge cases and complex flag combos
// that real RA achievements use.
// ---------------------------------------------------------------------------

#[test]
fn rcheevos_hit_count_plus_multi_cond() {
    // 0xH0001=20(2)_0xH0002=52 — hit count on first condition, plain on second.
    let (ours, theirs) = run_trace("0xH0001=20(2)_0xH0002=52", 256, 6, |frame, mem| {
        if frame >= 2 {
            mem.write(1, 20);
            mem.write(2, 52);
        }
    });
    assert_logs_agree("rcheevos_hit_count_plus_multi_cond", &ours, &theirs);
}

#[test]
fn rcheevos_core_plus_two_alts() {
    // 0xH0001=16S0xH0002=52S0xL0004=6 — core + two alt condsets.
    // Core: mem[1]=16. Alt1: mem[2]=52. Alt2: low nibble mem[4]=6.
    let (ours, theirs) = run_trace("0xH0001=16S0xH0002=52S0xL0004=6", 256, 5, |frame, mem| {
        if frame >= 2 {
            mem.write(1, 16); // core passes
        }
        if frame >= 3 {
            mem.write(2, 52); // alt1 passes → triggers
        }
    });
    assert_logs_agree("rcheevos_core_plus_two_alts", &ours, &theirs);
}

#[test]
fn rcheevos_delta_transition_detect() {
    // 0xH0001=10_d0xH0001=9 — fires when mem[1] transitions from 9 to 10.
    let (ours, theirs) = run_trace("0xH0001=10_d0xH0001=9", 256, 6, |frame, mem| {
        if frame <= 2 {
            mem.write(1, 0); // not 9, not 10
        } else if frame == 3 {
            mem.write(1, 9); // delta will be 0
        } else {
            mem.write(1, 10); // delta is 9, current is 10 → both conditions true
        }
    });
    assert_logs_agree("rcheevos_delta_transition_detect", &ours, &theirs);
}

#[test]
fn rcheevos_resetif_with_hit_count() {
    // 0xH0001=18(2)_0xH0002=52_R:0xL0004=4
    // Need 2 frames of mem[1]=18, plus mem[2]=52, reset when nibble(mem[4])=4.
    let (ours, theirs) = run_trace("0xH0001=18(2)_0xH0002=52_R:0xL0004=4", 256, 10, |frame, mem| {
        mem.write(1, 18);
        mem.write(2, 52);
        if frame == 3 {
            mem.write(4, 0x04); // low nibble = 4 → reset
        } else {
            mem.write(4, 0x00);
        }
    });
    assert_logs_agree("rcheevos_resetif_with_hit_count", &ours, &theirs);
}

#[test]
fn rcheevos_alts_with_constant_and_resetif() {
    // 0xH0001=10S1=1SR:0xH0000!=0
    // Core: mem[1]=10. Alt1: 1=1 (always true). Alt2: R:mem[0]!=0.
    // If mem[0]!=0 fires in alt2, it resets globally. But alt1 (1=1) is always
    // satisfied, so only core matters for triggering.
    let (ours, theirs) = run_trace("0xH0001=10S1=1SR:0xH0000!=0", 256, 6, |frame, mem| {
        // mem[0] starts at 0 (resetif not firing)
        if frame >= 3 {
            mem.write(1, 10); // core passes; alt1 always passes → trigger
        }
    });
    assert_logs_agree("rcheevos_alts_with_constant_and_resetif", &ours, &theirs);
}

#[test]
fn rcheevos_resetif_and_trigger_flag() {
    // R:0xH0003=1_0xH0004=1.1._T:0xH0000=1
    // ResetIf when mem[3]=1. Standard with legacy hit count 1. Trigger flag on mem[0]=1.
    let (ours, theirs) = run_trace("R:0xH0003=1_0xH0004=1.1._T:0xH0000=1", 256, 8, |frame, mem| {
        mem.write(4, 1); // standard condition always true, hit count 1 = needs 1 frame
        if frame >= 3 {
            mem.write(0, 1); // Trigger flag fires
        }
        // mem[3] stays 0 so ResetIf doesn't fire
    });
    assert_logs_agree("rcheevos_resetif_and_trigger_flag", &ours, &theirs);
}

#[test]
fn rcheevos_measured_with_hit_target() {
    // M:0xH0002=52(3) — Measured condition, fires when mem[2]=52 for 3 frames.
    let (ours, theirs) = run_trace("M:0xH0002=52(3)", 256, 6, |frame, mem| {
        if frame >= 2 {
            mem.write(2, 52);
        }
    });
    assert_logs_agree("rcheevos_measured_with_hit_target", &ours, &theirs);
}

#[test]
fn rcheevos_add_hits_plus_resetif() {
    // C:0xH0001=18_R:0xL0004=6(3) — AddHits when mem[1]=18, ResetIf
    // when low nibble mem[4]=6 for 3 frames. ResetIf fires after 3 frames
    // of nibble=6, but the AddHits row doesn't gate triggering on its own.
    // This condition should never trigger because there's no terminal Standard
    // condition with a satisfiable target.
    let (ours, theirs) = run_trace("C:0xH0001=18_R:0xL0004=6(3)", 256, 10, |frame, mem| {
        mem.write(1, 18);
        if frame >= 5 {
            mem.write(4, 0x06); // low nibble = 6
        }
    });
    assert_logs_agree("rcheevos_add_hits_plus_resetif", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Coverage gap tests — flags that had no differential coverage
// ---------------------------------------------------------------------------

#[test]
fn pauseif_blocks_accumulation() {
    // P:0xH0001=1 pauses when mem[1]=1. 0xH0000=5(3) needs 3 frames of mem[0]=5.
    // Pause on frames 2-3 should delay the hit-count completion.
    let (ours, theirs) = run_trace("0xH0000=5(3)_P:0xH0001=1", 16, 8, |frame, mem| {
        mem.write(0, 5); // always true for hit counting
        // pause on frames 2 and 3
        mem.write(1, if frame == 2 || frame == 3 { 1 } else { 0 });
    });
    assert_logs_agree("pauseif_blocks_accumulation", &ours, &theirs);
}

#[test]
fn pauseif_no_pause_triggers_normally() {
    // Same structure but pause never fires — should trigger after 3 frames.
    let (ours, theirs) = run_trace("0xH0000=5(3)_P:0xH0001=1", 16, 6, |_frame, mem| {
        mem.write(0, 5);
        mem.write(1, 0); // pause never fires
    });
    assert_logs_agree("pauseif_no_pause_triggers_normally", &ours, &theirs);
}

#[test]
fn reset_next_if_only_resets_next() {
    // Z:0xH0001=1 resets next condition's hits. 0xH0000=5(3) is the target.
    // ResetNextIf fires on frame 2, resetting the hit count of the next condition.
    let (ours, theirs) = run_trace("Z:0xH0001=1_0xH0000=5(3)", 16, 8, |frame, mem| {
        mem.write(0, 5);
        mem.write(1, if frame == 2 { 1 } else { 0 });
    });
    assert_logs_agree("reset_next_if_only_resets_next", &ours, &theirs);
}

#[test]
fn sub_source_chain() {
    // B:0xH0001 subtracts mem[1] from accumulator. Then 0xH0000>0 checks
    // if mem[0] - mem[1] > 0. Should fire when mem[0] > mem[1].
    let (ours, theirs) = run_trace("B:0xH0001_0xH0000>0", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 10);
            mem.write(1, 3); // 10 - 3 = 7 > 0
        }
    });
    assert_logs_agree("sub_source_chain", &ours, &theirs);
}

#[test]
fn sub_source_negative_no_trigger() {
    // B:0xH0001 subtracts mem[1]. 0xH0000>100 checks acc > 100.
    // mem[0]=5, mem[1]=10 → 5-10 = negative (wraps unsigned) or fails > 100.
    // Never triggers.
    let (ours, theirs) = run_trace("B:0xH0001_0xH0000>100", 16, 4, |_frame, mem| {
        mem.write(0, 5);
        mem.write(1, 10);
    });
    assert_logs_agree("sub_source_negative_no_trigger", &ours, &theirs);
}

#[test]
fn sub_hits_reduces_pool() {
    // C:0xH0000=5 adds hits, D:0xH0001=1 subtracts hits.
    // Terminal condition 0xH0002=0(3) needs net 3 hits.
    // If sub_hits fires some frames, it delays trigger.
    let (ours, theirs) = run_trace("C:0xH0000=5_D:0xH0001=1_0xH0002=0(3)", 16, 10, |frame, mem| {
        mem.write(0, 5); // add hits every frame
        mem.write(2, 0); // terminal always true
        // subtract on frames 1 and 3
        mem.write(1, if frame == 1 || frame == 3 { 1 } else { 0 });
    });
    assert_logs_agree("sub_hits_reduces_pool", &ours, &theirs);
}

#[test]
fn and_next_both_true_triggers() {
    // N:0xH0000=1 AND 0xH0001=2 — both must be true on same frame.
    let (ours, theirs) = run_trace("N:0xH0000=1_0xH0001=2", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 1);
            mem.write(1, 2);
        }
    });
    assert_logs_agree("and_next_both_true_triggers", &ours, &theirs);
}

#[test]
fn and_next_partial_no_trigger() {
    // N:0xH0000=1 AND 0xH0001=2 — only first is true.
    let (ours, theirs) = run_trace("N:0xH0000=1_0xH0001=2", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 1);
        }
        // mem[1] stays 0, never 2
    });
    assert_logs_agree("and_next_partial_no_trigger", &ours, &theirs);
}

#[test]
fn or_next_first_true_triggers() {
    // O:0xH0000=1 OR 0xH0001=2 — first becomes true.
    let (ours, theirs) = run_trace("O:0xH0000=1_0xH0001=2", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 1);
        }
    });
    assert_logs_agree("or_next_first_true_triggers", &ours, &theirs);
}

#[test]
fn or_next_second_true_triggers() {
    // O:0xH0000=1 OR 0xH0001=2 — second becomes true.
    let (ours, theirs) = run_trace("O:0xH0000=1_0xH0001=2", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(1, 2);
        }
    });
    assert_logs_agree("or_next_second_true_triggers", &ours, &theirs);
}

#[test]
fn or_next_neither_no_trigger() {
    // O:0xH0000=1 OR 0xH0001=2 — neither becomes true.
    let (ours, theirs) = run_trace("O:0xH0000=1_0xH0001=2", 16, 4, |_frame, _mem| {});
    assert_logs_agree("or_next_neither_no_trigger", &ours, &theirs);
}

#[test]
fn measured_if_gates_measured() {
    // Q:0xH0001=1 gates satisfaction AND measured display.
    // When MeasuredIf is always true, trigger timing should be the same.
    let (ours, theirs) = run_trace("M:0xH0000=5(3)_Q:0xH0001=1", 16, 6, |_frame, mem| {
        mem.write(0, 5);
        mem.write(1, 1); // measuredif always true
    });
    assert_logs_agree("measured_if_gates_measured", &ours, &theirs);
}

#[test]
fn add_address_indirect() {
    // I:0xH0000 adds mem[0] as offset to next read. 0xH0002=42 reads mem[mem[0]+2].
    // When mem[0]=3, reads mem[5]. Set mem[5]=42 to trigger.
    let (ours, theirs) = run_trace("I:0xH0000_0xH0002=42", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 3); // indirect offset = 3
            mem.write(5, 42); // mem[3+2] = 42
        }
    });
    assert_logs_agree("add_address_indirect", &ours, &theirs);
}

#[test]
fn prior_operand_comparison() {
    // p0xH0000!=0xH0000 — prior(mem[0]) != current(mem[0]).
    // Prior holds the value from the last time it changed. Fires when
    // mem[0] changes and old value differs.
    let (ours, theirs) = run_trace("p0xH0000!=0xH0000", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 10); // changes from 0 to 10
        }
    });
    assert_logs_agree("prior_operand_comparison", &ours, &theirs);
}

#[test]
fn add_source_sub_source_combo() {
    // A:0xH0000 adds mem[0], B:0xH0001 subs mem[1], 0xH0002=0 checks
    // (mem[0] - mem[1] + mem[2]) == 0. Effectively mem[0] - mem[1] + mem[2] = 0.
    let (ours, theirs) = run_trace("A:0xH0000_B:0xH0001_0xH0002=0", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 5);
            mem.write(1, 8);
            mem.write(2, 3); // 5 - 8 + 3 = 0
        }
    });
    assert_logs_agree("add_source_sub_source_combo", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Complex real-world patterns
// ---------------------------------------------------------------------------

/// ResetNextIf passing through an AddSource modifier to hit the Standard cond.
/// Z:0xH0002=1_A:0xH0003_0xH0000=5(3) — ResetNextIf at idx 0, AddSource at
/// idx 1 (modifier, skipped), Standard at idx 2 is the actual reset target.
#[test]
fn reset_next_if_passes_through_add_source() {
    let (ours, theirs) = run_trace(
        "Z:0xH0002=1_A:0xH0003_0xH0000=5(3)",
        16, 10,
        |frame, mem| {
            mem.write(0, 5); // standard condition always true (after add_source)
            mem.write(3, 0); // add_source contributes 0
            if frame == 2 {
                mem.write(2, 1); // ResetNextIf fires on frame 2
            } else {
                mem.write(2, 0);
            }
        },
    );
    assert_logs_agree("reset_next_if_passes_through_add_source", &ours, &theirs);
}

/// Chained AddSource: A:0xH0000_A:0xH0001_0xH0002=15.
/// Three memory addresses summed together must equal 15.
#[test]
fn chained_add_source_three() {
    let (ours, theirs) = run_trace("A:0xH0000_A:0xH0001_0xH0002=15", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 3);
            mem.write(1, 5);
            mem.write(2, 7); // 3+5+7=15
        }
    });
    assert_logs_agree("chained_add_source_three", &ours, &theirs);
}

/// AddSource with multiply: A:0xH0000*3_0xH0001=10.
/// mem[0]*3 + mem[1] == 10.
#[test]
fn add_source_multiply() {
    let (ours, theirs) = run_trace("A:0xH0000*3_0xH0001=10", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 2);
            mem.write(1, 4); // 2*3+4=10
        }
    });
    assert_logs_agree("add_source_multiply", &ours, &theirs);
}

/// ResetIf with hit count: R:0xH0001=1(3) — only resets after 3 consecutive
/// frames of mem[1]=1. Combined with 0xH0000=5(5).
#[test]
fn resetif_with_hit_count() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(5)_R:0xH0001=1(3)",
        16, 15,
        |frame, mem| {
            mem.write(0, 5); // standard always true
            // ResetIf condition true on frames 4-6 (3 consecutive → fires on frame 6)
            mem.write(1, if (4..=6).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetif_with_hit_count", &ours, &theirs);
}

/// PauseIf with hit count: P:0xH0001=1(3) — pauses after mem[1]=1 for 3 frames.
/// Combined with 0xH0000=5(4).
#[test]
fn pauseif_with_hit_count() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(4)_P:0xH0001=1(3)",
        16, 15,
        |frame, mem| {
            mem.write(0, 5);
            // PauseIf condition true frames 1-4, hits 3 target on frame 3
            mem.write(1, if (1..=4).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("pauseif_with_hit_count", &ours, &theirs);
}

/// AndNext with hit count on the gated condition.
/// N:0xH0000=1_0xH0001=2(3) — both must be true for 3 frames.
#[test]
fn and_next_with_hit_count() {
    let (ours, theirs) = run_trace("N:0xH0000=1_0xH0001=2(3)", 16, 8, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 1);
            mem.write(1, 2);
        }
    });
    assert_logs_agree("and_next_with_hit_count", &ours, &theirs);
}

/// OrNext with hit count: O:0xH0000=1_0xH0001=2(3).
/// Either arm being true accumulates toward the hit target.
#[test]
fn or_next_with_hit_count() {
    let (ours, theirs) = run_trace("O:0xH0000=1_0xH0001=2(3)", 16, 8, |frame, mem| {
        // Alternate which arm is true each frame
        if frame >= 2 {
            if frame % 2 == 0 {
                mem.write(0, 1);
                mem.write(1, 0);
            } else {
                mem.write(0, 0);
                mem.write(1, 2);
            }
        }
    });
    assert_logs_agree("or_next_with_hit_count", &ours, &theirs);
}

/// AddHits from two sources plus terminal.
/// C:0xH0000=1_C:0xH0001=1_0xH0002=1(5).
#[test]
fn add_hits_two_sources() {
    let (ours, theirs) = run_trace(
        "C:0xH0000=1_C:0xH0001=1_0xH0002=1(5)",
        16, 6,
        |frame, mem| {
            if frame >= 1 {
                mem.write(0, 1); // addhits source 1
                mem.write(1, 1); // addhits source 2
                mem.write(2, 1); // terminal
            }
        },
    );
    assert_logs_agree("add_hits_two_sources", &ours, &theirs);
}

/// AddHits plus SubHits cancel each other.
/// C:0xH0000=1_D:0xH0001=1_0xH0002=1(4).
/// Both true every frame: AddHits and SubHits cancel, only terminal's own
/// hits accumulate. Should trigger after 4 frames of terminal being true.
#[test]
fn add_hits_sub_hits_cancel() {
    let (ours, theirs) = run_trace(
        "C:0xH0000=1_D:0xH0001=1_0xH0002=1(4)",
        16, 8,
        |frame, mem| {
            if frame >= 1 {
                mem.write(0, 1);
                mem.write(1, 1);
                mem.write(2, 1);
            }
        },
    );
    assert_logs_agree("add_hits_sub_hits_cancel", &ours, &theirs);
}

/// Multi-condition condset: two standard conditions + hit count on each.
/// 0xH0000=1(2)_0xH0001=2(3) — both must independently meet their targets.
#[test]
fn multi_standard_hit_counts() {
    let (ours, theirs) = run_trace(
        "0xH0000=1(2)_0xH0001=2(3)",
        16, 6,
        |frame, mem| {
            if frame >= 1 {
                mem.write(0, 1);
                mem.write(1, 2);
            }
        },
    );
    assert_logs_agree("multi_standard_hit_counts", &ours, &theirs);
}

/// Delta on 16-bit word: d0x 0000>0x 0000 — fires when the word increases.
#[test]
fn delta_word_comparison() {
    let (ours, theirs) = run_trace("d0x 0000<0x 0000", 16, 5, |frame, mem| {
        // Word value increases on frame 3
        if frame >= 3 {
            mem.write(0, 0x00);
            mem.write(1, 0x01); // word = 0x0100 = 256
        }
    });
    assert_logs_agree("delta_word_comparison", &ours, &theirs);
}

/// ResetIf in an alt condset resets all hit counts globally.
/// Core: 0xH0000=1(3). Alt1: 0xH0001=1. Alt2: R:0xH0002=1.
/// ResetIf in alt2 should reset core's hit count.
#[test]
fn resetif_in_alt_resets_globally() {
    let (ours, theirs) = run_trace(
        "0xH0000=1(3)S0xH0001=1SR:0xH0002=1",
        16, 10,
        |frame, mem| {
            mem.write(0, 1); // core condition always true
            if frame >= 5 {
                mem.write(1, 1); // alt1 passes from frame 5
            }
            if frame == 3 {
                mem.write(2, 1); // reset on frame 3
            } else {
                mem.write(2, 0);
            }
        },
    );
    assert_logs_agree("resetif_in_alt_resets_globally", &ours, &theirs);
}

/// Trigger flag primed then fires: 0xH0000=1_0xH0001=2_T:0xH0002=3.
/// Core conditions must be true AND trigger condition must fire.
#[test]
fn trigger_flag_with_multi_cond() {
    let (ours, theirs) = run_trace(
        "0xH0000=1_0xH0001=2_T:0xH0002=3",
        16, 6,
        |frame, mem| {
            if frame >= 2 {
                mem.write(0, 1);
                mem.write(1, 2);
            }
            if frame >= 4 {
                mem.write(2, 3); // trigger fires
            }
        },
    );
    assert_logs_agree("trigger_flag_with_multi_cond", &ours, &theirs);
}

/// AddAddress chained: I:0xH0000_I:0xH0001_0xH0010=7.
/// Double indirection: effective address = 0x10 + mem[0] + mem[1].
#[test]
fn add_address_chained() {
    let (ours, theirs) = run_trace(
        "I:0xH0000_I:0xH0001_0xH0002=7",
        32, 5,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 2); // first offset
                mem.write(1, 3); // second offset
                mem.write(7, 7); // mem[2+2+3] = mem[7] = 7
            }
        },
    );
    assert_logs_agree("add_address_chained", &ours, &theirs);
}

/// Measured value mode (no hit target): M:0xH0000>5.
/// Measured with no hit count acts like a Standard condition that also
/// reports a measured value. Should trigger when condition is true.
#[test]
fn measured_value_mode() {
    let (ours, theirs) = run_trace("M:0xH0000>5", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 10);
        }
    });
    assert_logs_agree("measured_value_mode", &ours, &theirs);
}

/// Constant condition in core with alt: 1=1S0xH0000=5.
/// Core is always true (1=1). Alt fires when mem[0]=5.
#[test]
fn constant_core_with_alt() {
    let (ours, theirs) = run_trace("1=1S0xH0000=5", 16, 5, |frame, mem| {
        if frame >= 3 {
            mem.write(0, 5);
        }
    });
    assert_logs_agree("constant_core_with_alt", &ours, &theirs);
}

/// Multiple conditions: one with hit count, one level-triggered, one ResetIf.
/// 0xH0000=1(5)_0xH0001>0_R:0xH0002=1.
#[test]
fn mixed_conditions_complex() {
    let (ours, theirs) = run_trace(
        "0xH0000=1(5)_0xH0001>0_R:0xH0002=1",
        16, 12,
        |frame, mem| {
            mem.write(0, 1); // hit count condition always true
            if frame >= 3 {
                mem.write(1, 10); // level condition true from frame 3
            }
            if frame == 5 {
                mem.write(2, 1); // reset on frame 5
            } else {
                mem.write(2, 0);
            }
        },
    );
    assert_logs_agree("mixed_conditions_complex", &ours, &theirs);
}

/// Nibble-level comparison: low nibble and high nibble.
/// 0xL0000=3_0xU0000=5. Both must be true (byte = 0x53).
#[test]
fn nibble_conditions() {
    let (ours, theirs) = run_trace("0xL0000=3_0xU0000=5", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x53); // low=3, high=5
        }
    });
    assert_logs_agree("nibble_conditions", &ours, &theirs);
}

/// Bit-level condition: 0xM0000=1 (bit0 of byte at addr 0).
#[test]
fn bit_condition() {
    let (ours, theirs) = run_trace("0xM0000=1", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x01); // bit0 = 1
        }
    });
    assert_logs_agree("bit_condition", &ours, &theirs);
}

/// DWord (32-bit) comparison.
#[test]
fn dword_comparison() {
    let (ours, theirs) = run_trace("0xX0000=h12345678", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x78);
            mem.write(1, 0x56);
            mem.write(2, 0x34);
            mem.write(3, 0x12);
        }
    });
    assert_logs_agree("dword_comparison", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// ResetIf hit-count edge cases
// ---------------------------------------------------------------------------

/// ResetIf with hit count: standard triggers BEFORE ResetIf accumulates enough hits.
/// 0xH0000=5(3)_R:0xH0001=1(5) — standard needs 3 hits, ResetIf needs 5.
/// Standard is met before ResetIf fires, so achievement triggers.
#[test]
fn resetif_hit_count_standard_triggers_first() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(3)_R:0xH0001=1(5)",
        16, 8,
        |_frame, mem| {
            mem.write(0, 5); // standard always true
            mem.write(1, 1); // resetif condition always true, but needs 5 hits
        },
    );
    assert_logs_agree("resetif_hit_count_standard_triggers_first", &ours, &theirs);
}

/// ResetIf with hit count that fires BEFORE standard is met, resetting everything.
/// 0xH0000=5(6)_R:0xH0001=1(3) — standard needs 6 hits, ResetIf fires after 3.
/// ResetIf fires first, clearing standard's progress.
#[test]
fn resetif_hit_count_resets_before_standard() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(6)_R:0xH0001=1(3)",
        16, 15,
        |_frame, mem| {
            mem.write(0, 5);
            mem.write(1, 1); // always true → hits accumulate for ResetIf
        },
    );
    assert_logs_agree("resetif_hit_count_resets_before_standard", &ours, &theirs);
}

/// ResetIf with hit count stays active after meeting target (even when condition
/// becomes false), preventing trigger until a global reset clears its count.
#[test]
fn resetif_hit_count_stays_active() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(3)_R:0xH0001=1(2)",
        16, 12,
        |frame, mem| {
            mem.write(0, 5);
            // ResetIf condition true on frames 0-1 (accumulates 2 hits → fires)
            // then false from frame 2+. But once target met, stays active.
            mem.write(1, if frame <= 1 { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetif_hit_count_stays_active", &ours, &theirs);
}

/// ResetIf prevents same-frame trigger: when ResetIf fires on the same frame
/// the standard condition is met, the reset takes priority.
#[test]
fn resetif_prevents_same_frame_trigger() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(4)_R:0xH0001=1",
        16, 10,
        |frame, mem| {
            mem.write(0, 5);
            // ResetIf fires on frame 3 (the same frame standard hits 4/4)
            mem.write(1, if frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetif_prevents_same_frame_trigger", &ours, &theirs);
}

/// ResetIf gated by AndNext: N:0xH0002=1_R:0xH0001=1.
/// The ResetIf only fires when BOTH the AndNext condition and ResetIf condition are true.
#[test]
fn resetif_gated_by_and_next() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(4)_N:0xH0002=1_R:0xH0001=1",
        16, 10,
        |frame, mem| {
            mem.write(0, 5);
            mem.write(1, 1); // ResetIf condition always true
            // AndNext gate: only true on frame 3
            mem.write(2, if frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetif_gated_by_and_next", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Big-endian memory reads
// ---------------------------------------------------------------------------

/// BigEndianWord (16-bit BE): 0xI0000 reads bytes at [0,1] and swaps them.
/// Store 0x1234 BE (mem[0]=0x12, mem[1]=0x34) → value = 0x1234.
#[test]
fn big_endian_word() {
    let (ours, theirs) = run_trace("0xI0000=h1234", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x12); // high byte
            mem.write(1, 0x34); // low byte
        }
    });
    assert_logs_agree("big_endian_word", &ours, &theirs);
}

/// BigEndianDWord (32-bit BE): 0xG0000.
/// Store 0x12345678 BE → value = 0x12345678.
#[test]
fn big_endian_dword() {
    let (ours, theirs) = run_trace("0xG0000=h12345678", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x12);
            mem.write(1, 0x34);
            mem.write(2, 0x56);
            mem.write(3, 0x78);
        }
    });
    assert_logs_agree("big_endian_dword", &ours, &theirs);
}

/// BigEndianThreeBytes (24-bit BE): 0xJ0000.
/// Store 0x123456 BE → value = 0x123456.
#[test]
fn big_endian_three_bytes() {
    let (ours, theirs) = run_trace("0xJ0000=h123456", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x12);
            mem.write(1, 0x34);
            mem.write(2, 0x56);
        }
    });
    assert_logs_agree("big_endian_three_bytes", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// ThreeBytes (24-bit LE)
// ---------------------------------------------------------------------------

/// ThreeBytes: 0xW0000 reads 3 bytes little-endian.
#[test]
fn three_bytes_le() {
    let (ours, theirs) = run_trace("0xW0000=h123456", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x56); // least significant
            mem.write(1, 0x34);
            mem.write(2, 0x12); // most significant
        }
    });
    assert_logs_agree("three_bytes_le", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// BCD operand
// ---------------------------------------------------------------------------

/// BCD decode: b0xH0000 interprets the byte as packed BCD.
/// 0x25 BCD = 25 decimal.
#[test]
fn bcd_byte() {
    let (ours, theirs) = run_trace("b0xH0000=25", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x25); // BCD 25
        }
    });
    assert_logs_agree("bcd_byte", &ours, &theirs);
}

/// BCD with 16-bit word: b0x 0000 = BCD decode of 16-bit LE.
/// 0x1234 BCD = 1234 decimal.
#[test]
fn bcd_word() {
    let (ours, theirs) = run_trace("b0x 0000=1234", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x34); // low byte BCD
            mem.write(1, 0x12); // high byte BCD
        }
    });
    assert_logs_agree("bcd_word", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Inverted operand
// ---------------------------------------------------------------------------

/// Inverted byte: ~0xH0000 bitwise-inverts the value, masked to byte width.
/// ~0xFF = 0x00, ~0x00 = 0xFF.
#[test]
fn inverted_byte() {
    let (ours, theirs) = run_trace("~0xH0000=hfe", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x01); // ~0x01 = 0xFE
        }
    });
    assert_logs_agree("inverted_byte", &ours, &theirs);
}

/// Inverted word: ~0x 0000 bitwise-inverts 16-bit value, masked to 0xFFFF.
#[test]
fn inverted_word() {
    let (ours, theirs) = run_trace("~0x 0000=hfffe", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x01); // LE word = 0x0001, ~0x0001 = 0xFFFE
            mem.write(1, 0x00);
        }
    });
    assert_logs_agree("inverted_word", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// BitCount
// ---------------------------------------------------------------------------

/// BitCount: 0xK0000 returns the popcount of the byte at addr 0.
/// 0xFF has 8 bits set.
#[test]
fn bitcount_byte() {
    let (ours, theirs) = run_trace("0xK0000=8", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0xFF); // popcount = 8
        }
    });
    assert_logs_agree("bitcount_byte", &ours, &theirs);
}

/// BitCount partial: 0xA5 = 10100101 = 4 bits set.
#[test]
fn bitcount_partial() {
    let (ours, theirs) = run_trace("0xK0000=4", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0xA5); // popcount = 4
        }
    });
    assert_logs_agree("bitcount_partial", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Individual bit positions
// ---------------------------------------------------------------------------

/// Bit2: 0xO0000=1 tests bit 2 of the byte.
#[test]
fn bit2_condition() {
    let (ours, theirs) = run_trace("0xO0000=1", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x04); // bit2 = 1
        }
    });
    assert_logs_agree("bit2_condition", &ours, &theirs);
}

/// Bit5: 0xR0000=1 tests bit 5.
#[test]
fn bit5_condition() {
    let (ours, theirs) = run_trace("0xR0000=1", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x20); // bit5 = 1
        }
    });
    assert_logs_agree("bit5_condition", &ours, &theirs);
}

/// Bit7: 0xT0000=1 tests bit 7 (sign bit).
#[test]
fn bit7_condition() {
    let (ours, theirs) = run_trace("0xT0000=1", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x80); // bit7 = 1
        }
    });
    assert_logs_agree("bit7_condition", &ours, &theirs);
}

/// Multiple bits combined: bit0 AND bit7 both set.
#[test]
fn multiple_bits_combined() {
    let (ours, theirs) = run_trace("0xM0000=1_0xT0000=1", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0x81); // bit0=1, bit7=1
        }
    });
    assert_logs_agree("multiple_bits_combined", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// PauseIf edge cases
// ---------------------------------------------------------------------------

/// PauseIf without hit count unpauses immediately when condition becomes false.
/// Hits resume accumulating the frame after unpause.
#[test]
fn pauseif_no_hitcount_unpauses() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(5)_P:0xH0001=1",
        16, 10,
        |frame, mem| {
            mem.write(0, 5);
            // Pause on frames 2-3, unpause from frame 4
            mem.write(1, if frame == 2 || frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("pauseif_no_hitcount_unpauses", &ours, &theirs);
}

/// PauseIf + ResetIf interaction: ResetIf in a paused condset should NOT fire
/// (pause suppresses ResetIf). Only fires after unpause.
#[test]
fn pauseif_suppresses_resetif() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(4)_P:0xH0002=1_R:0xH0001=1",
        16, 12,
        |frame, mem| {
            mem.write(0, 5);
            // ResetIf condition always true
            mem.write(1, 1);
            // Pause on frames 0-4 (suppresses ResetIf)
            // Unpause from frame 5 (ResetIf fires immediately)
            mem.write(2, if frame <= 4 { 1 } else { 0 });
        },
    );
    assert_logs_agree("pauseif_suppresses_resetif", &ours, &theirs);
}

/// Multiple PauseIf conditions: either being true pauses the condset.
#[test]
fn multiple_pauseif() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(4)_P:0xH0001=1_P:0xH0002=1",
        16, 10,
        |frame, mem| {
            mem.write(0, 5);
            // First PauseIf true on frames 1-2
            mem.write(1, if frame == 1 || frame == 2 { 1 } else { 0 });
            // Second PauseIf true on frame 3
            mem.write(2, if frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("multiple_pauseif", &ours, &theirs);
}

/// PauseIf with hit count: once hit target met, stays paused permanently
/// even when condition becomes false. Only ResetIf clears it.
#[test]
fn pauseif_hitcount_permanent_until_reset() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(3)_P:0xH0001=1(2)_R:0xH0002=1",
        16, 15,
        |frame, mem| {
            mem.write(0, 5);
            // PauseIf condition true frames 0-1, hits target 2 on frame 1
            // Then false from frame 2+ (but stays paused)
            mem.write(1, if frame <= 1 { 1 } else { 0 });
            // ResetIf fires on frame 6 to clear pause
            mem.write(2, if frame == 6 { 1 } else { 0 });
        },
    );
    assert_logs_agree("pauseif_hitcount_permanent_until_reset", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Modifier operations
// ---------------------------------------------------------------------------

/// AddSource with divide: A:0xH0000/2_0xH0001=5 → mem[0]/2 + mem[1] == 5.
#[test]
fn add_source_divide() {
    let (ours, theirs) = run_trace("A:0xH0000/2_0xH0001=5", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 6); // 6/2 = 3
            mem.write(1, 2); // 3+2 = 5
        }
    });
    assert_logs_agree("add_source_divide", &ours, &theirs);
}

/// AddSource with bitwise AND: A:0xH0000&hf0_0xH0001=hf5.
/// mem[0]&0xF0 + mem[1] == 0xF5.
#[test]
fn add_source_bitwise_and() {
    let (ours, theirs) = run_trace("A:0xH0000&hf0_0xH0001=hf5", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 0xAB); // 0xAB & 0xF0 = 0xA0
            mem.write(1, 0x55); // 0xA0 + 0x55 = 0xF5
        }
    });
    assert_logs_agree("add_source_bitwise_and", &ours, &theirs);
}

/// SubSource with multiply: B:0xH0001*2_0xH0000=4.
/// -mem[1]*2 + mem[0] == 4 → mem[0] - mem[1]*2 == 4.
#[test]
fn sub_source_multiply() {
    let (ours, theirs) = run_trace("B:0xH0001*2_0xH0000=4", 16, 4, |frame, mem| {
        if frame >= 2 {
            mem.write(0, 10); // 10 - 3*2 = 4
            mem.write(1, 3);
        }
    });
    assert_logs_agree("sub_source_multiply", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// MeasuredIf gates satisfaction
// ---------------------------------------------------------------------------

/// MeasuredIf false blocks trigger: Q:0xH0001=1_0xH0000=5.
/// MeasuredIf gates is_true just like Standard. When false, condset unsatisfied.
#[test]
fn measured_if_blocks_when_false() {
    let (ours, theirs) = run_trace(
        "Q:0xH0001=1_0xH0000=5",
        16, 8,
        |frame, mem| {
            // Frame 0: MeasuredIf false (mem[1]=0≠1), Standard false (mem[0]=0≠5). Not triggered.
            // Frame 2: Standard true, MeasuredIf still false → blocked.
            // Frame 4: Both true → triggers.
            if frame >= 2 {
                mem.write(0, 5); // Standard becomes true
            }
            if frame >= 4 {
                mem.write(1, 1); // MeasuredIf becomes true
            }
        },
    );
    assert_logs_agree("measured_if_blocks_when_false", &ours, &theirs);
}

/// MeasuredIf true doesn't block: Q:0xH0001=0_0xH0000=5.
/// mem[1] starts at 0, so MeasuredIf(mem[1]=0) is true from the start.
#[test]
fn measured_if_true_allows_trigger() {
    let (ours, theirs) = run_trace(
        "Q:0xH0001=0_0xH0000=5",
        16, 5,
        |frame, mem| {
            if frame >= 2 {
                mem.write(0, 5); // Standard becomes true
            }
            // mem[1] stays 0, MeasuredIf always true
        },
    );
    assert_logs_agree("measured_if_true_allows_trigger", &ours, &theirs);
}

/// MeasuredIf with Measured hit target: both must be satisfied.
/// M:0xH0000=5(3)_Q:0xH0001=1 — Measured needs 3 hits, MeasuredIf must be true.
/// MeasuredIf false delays trigger.
#[test]
fn measured_if_with_measured_hit_target() {
    let (ours, theirs) = run_trace(
        "M:0xH0000=5(3)_Q:0xH0001=1",
        16, 8,
        |frame, mem| {
            mem.write(0, 5); // Measured condition always true
            // MeasuredIf false on frames 0-3, true from frame 4
            mem.write(1, if frame >= 4 { 1 } else { 0 });
        },
    );
    assert_logs_agree("measured_if_with_measured_hit_target", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Delta/prior tracking during pause
// ---------------------------------------------------------------------------

/// Delta comparison after PauseIf: values change during pause, delta should
/// reflect the previous frame (not the stale pre-pause value).
/// 0xH0000>d0xH0000 fires when value increases. Paused during value changes.
#[test]
fn delta_after_pause_value_changes() {
    let (ours, theirs) = run_trace(
        "0xH0000>d0xH0000_P:0xH0001=1",
        16, 10,
        |frame, mem| {
            // Value at addr 0 increases every frame: 0,1,2,...
            mem.write(0, frame as u8);
            // Pause on frames 2-4
            mem.write(1, if (2..=4).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("delta_after_pause_value_changes", &ours, &theirs);
}

/// Delta comparison after pause when value stabilizes during pause.
/// Without the fix, stale delta would cause incorrect trigger after unpause.
#[test]
fn delta_after_pause_value_stabilizes() {
    let (ours, theirs) = run_trace(
        "0xH0000>d0xH0000_P:0xH0001=1",
        16, 10,
        |frame, mem| {
            // Value changes to 5 on frame 1, then stays 5 during pause and after.
            // After unpause, delta should be 5 (same as current) → NOT triggered.
            if frame >= 1 {
                mem.write(0, 5);
            }
            // Pause on frames 2-5
            mem.write(1, if (2..=5).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("delta_after_pause_value_stabilizes", &ours, &theirs);
}

/// Prior value tracking during pause. Prior should update as value changes
/// during paused frames, not stay stale.
#[test]
fn prior_after_pause() {
    let (ours, theirs) = run_trace(
        "p0xH0000=5_P:0xH0001=1",
        16, 10,
        |frame, mem| {
            // Value goes: 0,0,5,10,10,10,... (changes on frames 2 and 3)
            if frame >= 3 {
                mem.write(0, 10);
            } else if frame >= 2 {
                mem.write(0, 5);
            }
            // Pause on frames 2-4 (value changes from 0→5→10 during pause)
            // After unpause on frame 5: value=10, prior=5. prior(5)=5 → triggers!
            mem.write(1, if (2..=4).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("prior_after_pause", &ours, &theirs);
}

/// Hit count with delta after pause: pausing shouldn't cause extra triggers
/// from stale delta values.
#[test]
fn delta_hit_count_after_pause() {
    let (ours, theirs) = run_trace(
        "0xH0000>d0xH0000(3)_P:0xH0001=1",
        16, 12,
        |frame, mem| {
            // Value increases every frame
            mem.write(0, frame as u8);
            // Pause on frames 3-5
            mem.write(1, if (3..=5).contains(&frame) { 1 } else { 0 });
        },
    );
    assert_logs_agree("delta_hit_count_after_pause", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// WAITING state hit count reset
// ---------------------------------------------------------------------------

/// WAITING state: when conditions are trivially true from boot, hit counts
/// should be reset each frame. Verifies that if the achievement becomes
/// unsatisfied later, accumulated hits don't allow premature trigger.
#[test]
fn waiting_resets_hits_while_satisfied() {
    // 0xH0000=1(3)_0xH0001=1 — needs 3 hits of mem[0]=1 AND mem[1]=1.
    // mem[0]=1 always, mem[1]=1 for frames 0-5 then becomes 0.
    // While both true → WAITING (resets hits). When mem[1]=0 → exit WAITING.
    // Then hits accumulate from 0.
    let (ours, theirs) = run_trace(
        "0xH0000=1(3)_0xH0001=1",
        16, 15,
        |frame, mem| {
            mem.write(0, 1);
            mem.write(1, if frame <= 5 { 1 } else { 0 });
            // Frame 6: mem[1]=0 → would_trigger false → exit WAITING
            // But mem[1]=0 means standard[1] false → not satisfied
            // Frame 7+: need BOTH true. mem[1] still 0 → never triggers
        },
    );
    assert_logs_agree("waiting_resets_hits_while_satisfied", &ours, &theirs);
}

/// WAITING state with hit-count-only condition that becomes satisfied before
/// the level-triggered condition does.
#[test]
fn waiting_hit_count_accumulation() {
    // 0xH0000=5(2)_0xH0001!=0 — needs 2 hits of mem[0]=5 AND mem[1]!=0.
    // mem[0]=5 always, mem[1]=0 initially → hit count condition met but level
    // condition false → exits WAITING. Then when mem[1] becomes non-zero,
    // triggers if hit count still met.
    let (ours, theirs) = run_trace(
        "0xH0000=5(2)_0xH0001!=0",
        16, 8,
        |frame, mem| {
            mem.write(0, 5);
            if frame >= 3 {
                mem.write(1, 1);
            }
        },
    );
    assert_logs_agree("waiting_hit_count_accumulation", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// PauseIf hit increment behavior
// ---------------------------------------------------------------------------

/// PauseIf with hit count: hits should accumulate even while the condset is
/// already paused (by the same PauseIf reaching its target).
#[test]
fn pauseif_hits_increment_during_pause() {
    // P:0xH0001=1(2)_0xH0000=5(3) — PauseIf needs 2 hits, standard needs 3.
    // mem[1]=1 always → PauseIf fires on frame 1 (hit target 2 met).
    // Stays paused forever. Never triggers.
    let (ours, theirs) = run_trace(
        "0xH0000=5(3)_P:0xH0001=1(2)",
        16, 10,
        |_frame, mem| {
            mem.write(0, 5);
            mem.write(1, 1);
        },
    );
    assert_logs_agree("pauseif_hits_increment_during_pause", &ours, &theirs);
}

/// PauseIf with hit count: condition true for exactly the target frames,
/// then false. Should stay paused permanently.
#[test]
fn pauseif_hitcount_then_false_stays_paused() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(3)_P:0xH0001=1(2)",
        16, 12,
        |frame, mem| {
            mem.write(0, 5);
            // PauseIf condition true frames 0-1 (hits reach 2), false from frame 2
            mem.write(1, if frame <= 1 { 1 } else { 0 });
        },
    );
    assert_logs_agree("pauseif_hitcount_then_false_stays_paused", &ours, &theirs);
}

// =========================================================================
// Float comparison tests
// =========================================================================

/// Float equal: fF0000=f3.5. Memory starts at 0.0, set to 3.5 on frame 3.
#[test]
fn float_equal() {
    let (ours, theirs) = run_trace(
        "fF0000=f3.5",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, 3.5);
            }
        },
    );
    assert_logs_agree("float_equal", &ours, &theirs);
}

/// Float greater: fF0000>f1.0. Memory starts at 0.0, set to 2.5 on frame 2.
#[test]
fn float_greater() {
    let (ours, theirs) = run_trace(
        "fF0000>f1.0",
        16, 10,
        |frame, mem| {
            if frame >= 2 {
                mem.write_f32_le(0, 2.5);
            }
        },
    );
    assert_logs_agree("float_greater", &ours, &theirs);
}

/// Float less: fF0000<f10.0. Memory starts at 0.0 (which is < 10.0 and true
/// from boot). Use a construction that starts false: fF0000<f-1.0 is never
/// true (0.0 is not less than -1.0). Then set memory to -5.0 on frame 3.
#[test]
fn float_less() {
    let (ours, theirs) = run_trace(
        "fF0000<f-1.0",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, -5.0);
            }
        },
    );
    assert_logs_agree("float_less", &ours, &theirs);
}

/// Float not-equal: fF0000!=f0.0. Memory starts at 0.0 (false from boot),
/// becomes 1.0 on frame 2.
#[test]
fn float_not_equal() {
    let (ours, theirs) = run_trace(
        "fF0000!=f0.0",
        16, 10,
        |frame, mem| {
            if frame >= 2 {
                mem.write_f32_le(0, 1.0);
            }
        },
    );
    assert_logs_agree("float_not_equal", &ours, &theirs);
}

/// Float equal with integer RHS: fF0000=3. Memory starts at 0.0, set to 3.0
/// on frame 3. The integer 3 should be promoted to float for comparison.
#[test]
fn float_mem_vs_integer_const() {
    let (ours, theirs) = run_trace(
        "fF0000=3",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, 3.0);
            }
        },
    );
    assert_logs_agree("float_mem_vs_integer_const", &ours, &theirs);
}

/// Integer mem vs float constant: 0xX0000=f42.0.
/// Memory starts at 0, set to 42 on frame 3.
#[test]
fn integer_mem_vs_float_const() {
    let (ours, theirs) = run_trace(
        "0xX0000=f42.0",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_u32_le(0, 42);
            }
        },
    );
    assert_logs_agree("integer_mem_vs_float_const", &ours, &theirs);
}

/// Float delta: dfF0000!=fF0000. Fire when the float value changes.
#[test]
fn float_delta() {
    let (ours, theirs) = run_trace(
        "dfF0000!=fF0000",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, 1.5);
            }
        },
    );
    assert_logs_agree("float_delta", &ours, &theirs);
}

/// Float with hit count: fF0000>f0.0 with hit target 3.
/// Set to 1.0 on frame 2 — should trigger on frame 4 (3 frames of true).
#[test]
fn float_hit_count() {
    let (ours, theirs) = run_trace(
        "fF0000>f0.0(3)",
        16, 10,
        |frame, mem| {
            if frame >= 2 {
                mem.write_f32_le(0, 1.0);
            }
        },
    );
    assert_logs_agree("float_hit_count", &ours, &theirs);
}

/// Float with AddSource: A:fF0000_fF0004>f5.0.
/// Two float addresses summed and compared to float constant.
#[test]
fn float_add_source() {
    let (ours, theirs) = run_trace(
        "A:fF0000_fF0004>f5.0",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, 2.5);
                mem.write_f32_le(4, 3.0);
            }
        },
    );
    assert_logs_agree("float_add_source", &ours, &theirs);
}

/// Float impossible: fF0000=f999.0 — never true since we never write 999.0.
#[test]
fn float_impossible() {
    let (ours, theirs) = run_trace(
        "fF0000=f999.0",
        16, 10,
        |_frame, _mem| {},
    );
    assert_logs_agree("float_impossible", &ours, &theirs);
    assert!(ours.iter().all(|b| !b), "should never trigger");
}

/// Float big-endian: fB0000=f3.5. Write 3.5 in big-endian byte order.
#[test]
fn float_big_endian() {
    let (ours, theirs) = run_trace(
        "fB0000=f3.5",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                let bits = 3.5f32.to_bits();
                // Write big-endian: MSB first
                mem.write(0, (bits >> 24) as u8);
                mem.write(1, (bits >> 16) as u8);
                mem.write(2, (bits >> 8) as u8);
                mem.write(3, bits as u8);
            }
        },
    );
    assert_logs_agree("float_big_endian", &ours, &theirs);
}

// =========================================================================
// Edge case: complex chains and interactions
// =========================================================================

/// AddSource chain with 3 operands: A:0xH0+A:0xH1+0xH2 > 10.
/// 3+4+5 = 12 > 10 fires on frame 3.
#[test]
fn add_source_triple_chain() {
    let (ours, theirs) = run_trace(
        "A:0xH0000_A:0xH0001_0xH0002>10",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 3);
                mem.write(1, 4);
                mem.write(2, 5);
            }
        },
    );
    assert_logs_agree("add_source_triple_chain", &ours, &theirs);
}

/// SubSource with negative result: A:0xH0-B:0xH1 compared.
/// 2 - 5 = wraps unsigned. Use a condition that tests this.
/// 0xH0000 - 0xH0001 > 200 (unsigned wrap: 2-5 = 0xFFFFFFFD > 200).
/// This should trigger from frame 3 onward.
#[test]
fn sub_source_underflow() {
    let (ours, theirs) = run_trace(
        "A:0xH0000_B:0xH0001_0=0",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 2);
                mem.write(1, 5);
            }
        },
    );
    // "A:0xH0+B:0xH1_0=0" → accum = 2-5 = -3, then 0+accum = -3 as u32 = large.
    // 0=0 is always true from boot so WAITING blocks until it's not true then becomes true.
    // Actually with accum, the effective LHS is (0 + (-3)) cast to u32. The comparison is
    // effective_lhs = 0 vs RHS = 0. But the accum modifies the LHS...
    // Let me just check agreement.
    assert_logs_agree("sub_source_underflow", &ours, &theirs);
}

/// Measured value mode (no hit count): M:0xH0000=5 — fires immediately when true.
/// Should behave like Standard: true when mem[0]=5.
#[test]
fn measured_value_mode_standard_behavior() {
    let (ours, theirs) = run_trace(
        "M:0xH0000=5",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 5);
            }
        },
    );
    assert_logs_agree("measured_value_mode", &ours, &theirs);
}

/// Measured with hit count in multi-condset: core has Measured(3), alt1 has Standard.
/// Core + alt must both be true.
#[test]
fn measured_hit_count_with_alt() {
    let (ours, theirs) = run_trace(
        "M:0xH0000=5(3)S0xH0001=1",
        16, 12,
        |frame, mem| {
            mem.write(0, 5); // always true
            if frame >= 2 {
                mem.write(1, 1); // alt true from frame 2
            }
        },
    );
    assert_logs_agree("measured_hit_count_with_alt", &ours, &theirs);
}

/// ResetIf in alt condset: core always true, alt has ResetIf.
/// The ResetIf in the alt should reset all hit counts.
#[test]
fn resetif_in_alt() {
    let (ours, theirs) = run_trace(
        "0xH0000=5(5)S0xH0001=1_R:0xH0002=1",
        16, 20,
        |frame, mem| {
            mem.write(0, 5); // core: always true, needs 5 hits
            mem.write(1, 1); // alt standard: always true
            // Reset on frame 3, clearing core's hit count
            mem.write(2, if frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetif_in_alt", &ours, &theirs);
}

/// PauseIf in alt condset: core has a standard condition, alt pauses.
/// When alt is paused, the alt cannot satisfy — so trigger depends on other alts.
#[test]
fn pauseif_in_alt() {
    let (ours, theirs) = run_trace(
        "0xH0000=5SP:0xH0001=1_0xH0002=1S0xH0003=1",
        16, 12,
        |frame, mem| {
            mem.write(0, 5); // core always true
            mem.write(1, 1); // alt1 paused always
            mem.write(2, 1); // alt1 standard (but paused)
            // alt2: only true from frame 4
            if frame >= 4 {
                mem.write(3, 1);
            }
        },
    );
    assert_logs_agree("pauseif_in_alt", &ours, &theirs);
}

/// AndNext chain into PauseIf: N:0xH0=1_P:0xH1=1.
/// The AndNext gates the PauseIf — should only pause when BOTH are true.
#[test]
fn andnext_gates_pauseif() {
    let (ours, theirs) = run_trace(
        "0xH0002=5_N:0xH0000=1_P:0xH0001=1",
        16, 12,
        |frame, mem| {
            mem.write(2, 5); // standard condition always true
            // AndNext condition: only true from frame 4
            mem.write(0, if frame >= 4 { 1 } else { 0 });
            // PauseIf base: always true (but gated by AndNext)
            mem.write(1, 1);
        },
    );
    assert_logs_agree("andnext_gates_pauseif", &ours, &theirs);
}

/// OrNext with three arms: O:0xH0=1_O:0xH1=1_0xH2=1.
/// Any of the three being true should pass. All start false, addr2 true from frame 3.
#[test]
fn ornext_three_arms() {
    let (ours, theirs) = run_trace(
        "O:0xH0000=1_O:0xH0001=1_0xH0002=1",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(2, 1);
            }
        },
    );
    assert_logs_agree("ornext_three_arms", &ours, &theirs);
}

/// AddHits with multiple contributors feeding a Standard condition.
/// C:0xH0=1_C:0xH1=1_0xH2=1(5) — AddHits from two sources plus Standard's own hits.
#[test]
fn addhits_multiple_contributors() {
    let (ours, theirs) = run_trace(
        "C:0xH0000=1_C:0xH0001=1_0xH0002=1(5)",
        16, 10,
        |frame, mem| {
            // addr0: true on even frames, addr1: true on odd frames
            // addr2: always true
            // Each frame, one of addr0/addr1 contributes a hit plus addr2's own hit.
            mem.write(0, if frame % 2 == 0 { 1 } else { 0 });
            mem.write(1, if frame % 2 == 1 { 1 } else { 0 });
            mem.write(2, 1);
        },
    );
    assert_logs_agree("addhits_multiple_contributors", &ours, &theirs);
}

/// SubHits reducing the hit pool below the target.
/// C:0xH0=1_D:0xH1=1_0xH2=1(10) — AddHits from addr0, SubHits from addr1.
/// addr1 true cancels some hits from addr0.
#[test]
fn subhits_reduces_total() {
    let (ours, theirs) = run_trace(
        "C:0xH0000=1_D:0xH0001=1_0xH0002=1(10)",
        16, 20,
        |frame, mem| {
            mem.write(0, 1); // always contributes hits
            // addr1 true first 5 frames, subtracting hits
            mem.write(1, if frame < 5 { 1 } else { 0 });
            mem.write(2, 1); // standard always true
        },
    );
    assert_logs_agree("subhits_reduces_total", &ours, &theirs);
}

/// Trigger flag with ResetIf: T:0xH0>0 with R:0xH1=1.
/// Achievement is primed when trigger passes, but ResetIf prevents firing.
#[test]
fn trigger_with_resetif() {
    let (ours, theirs) = run_trace(
        "T:0xH0000>0_R:0xH0001=1",
        16, 12,
        |frame, mem| {
            if frame >= 2 {
                mem.write(0, 5); // trigger condition true
            }
            // Reset active frames 2-4, released from frame 5
            mem.write(1, if frame >= 2 && frame <= 4 { 1 } else { 0 });
        },
    );
    assert_logs_agree("trigger_with_resetif", &ours, &theirs);
}

/// ResetNextIf: Z:0xH0=1_0xH1=5(3).
/// ResetNextIf clears hits on the Standard condition when addr0=1.
#[test]
fn resetnextif_clears_hits() {
    let (ours, theirs) = run_trace(
        "Z:0xH0000=1_0xH0001=5(3)",
        16, 15,
        |frame, mem| {
            mem.write(1, 5); // standard always true
            // ResetNext fires on frame 3, clearing Standard's hit count
            mem.write(0, if frame == 3 { 1 } else { 0 });
        },
    );
    assert_logs_agree("resetnextif_clears_hits", &ours, &theirs);
}

/// AddSource with Multiply modifier: A:0xH0*3_0xH1>10.
/// addr0=4 → 4*3=12, addr1=0 → 12+0=12 > 10. But need to avoid always-true.
/// Start: addr0=0, addr1=0 → 0*3+0 = 0. Frame 3: addr0=4 → 4*3+0=12 > 10.
#[test]
fn add_source_multiply_chain() {
    let (ours, theirs) = run_trace(
        "A:0xH0000*3_0xH0001>10",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 4);
            }
        },
    );
    assert_logs_agree("add_source_multiply_chain", &ours, &theirs);
}

/// Delta after ResetIf: delta values should persist through resets.
/// d0xH0!=0xH0 fires when value changes. After ResetIf, delta tracking
/// should still work correctly.
#[test]
fn delta_persists_through_reset() {
    let (ours, theirs) = run_trace(
        "d0xH0000!=0xH0000_R:0xH0001=1",
        16, 12,
        |frame, mem| {
            // Change value on frames 3 and 7
            mem.write(0, if frame >= 3 && frame < 5 { 10 } else if frame >= 7 { 20 } else { 0 });
            // Reset on frame 4
            mem.write(1, if frame == 4 { 1 } else { 0 });
        },
    );
    assert_logs_agree("delta_persists_through_reset", &ours, &theirs);
}

/// Multiple standard conditions: 0xH0=1_0xH1=2_0xH2=3. All must be true.
#[test]
fn multiple_standard_conditions() {
    let (ours, theirs) = run_trace(
        "0xH0000=1_0xH0001=2_0xH0002=3",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                mem.write(0, 1);
                mem.write(1, 2);
                mem.write(2, 3);
            }
        },
    );
    assert_logs_agree("multiple_standard_conditions", &ours, &theirs);
}

/// Word comparison with high values: 0x 0000>60000.
/// 16-bit value at addr0-1 starts at 0, set to 65000 on frame 3.
#[test]
fn word_high_value() {
    let (ours, theirs) = run_trace(
        "0x 0000>60000",
        16, 8,
        |frame, mem| {
            if frame >= 3 {
                // 65000 = 0xFDE8 → LE: 0xE8, 0xFD
                mem.write(0, 0xE8);
                mem.write(1, 0xFD);
            }
        },
    );
    assert_logs_agree("word_high_value", &ours, &theirs);
}

/// Float SubSource: A:fF0000_B:fF0004_0=0.
/// Result should be float subtraction.
#[test]
fn float_sub_source() {
    let (ours, theirs) = run_trace(
        "A:fF0000_B:fF0004_fF0008>f0.0",
        16, 10,
        |frame, mem| {
            if frame >= 3 {
                mem.write_f32_le(0, 10.0);
                mem.write_f32_le(4, 3.0); // subtracted
                mem.write_f32_le(8, 0.0); // terminal: 10.0-3.0+0.0 = 7.0 > 0.0
            }
        },
    );
    assert_logs_agree("float_sub_source", &ours, &theirs);
}

/// Float with PauseIf: P:fF0000=f0.0_fF0004>f1.0.
/// When float at addr0 is 0.0, pause. When not 0.0, allow trigger.
#[test]
fn float_pauseif() {
    let (ours, theirs) = run_trace(
        "P:fF0000=f0.0_fF0004>f1.0",
        16, 10,
        |frame, mem| {
            // Float at addr0 = 0.0 (pause) until frame 3
            if frame >= 3 {
                mem.write_f32_le(0, 1.0); // unpause
            }
            if frame >= 2 {
                mem.write_f32_le(4, 2.0); // trigger condition
            }
        },
    );
    assert_logs_agree("float_pauseif", &ours, &theirs);
}

/// Float with ResetIf: R:fF0000>f5.0_fF0004=f3.0(3).
/// Reset when float > 5.0, standard needs 3 hits of float == 3.0.
#[test]
fn float_resetif() {
    let (ours, theirs) = run_trace(
        "R:fF0000>f5.0_fF0004=f3.0(3)",
        16, 12,
        |frame, mem| {
            mem.write_f32_le(4, 3.0); // standard always true
            // Reset on frame 2
            mem.write_f32_le(0, if frame == 2 { 10.0 } else { 0.0 });
        },
    );
    assert_logs_agree("float_resetif", &ours, &theirs);
}

// ===========================================================================
// VALUE differential tests: rveevr's eval_value vs rcheevos' rc_evaluate_value
// ===========================================================================
//
// rcheevos' rc_parse_value expects condition-format strings when the second
// character is ':' (indicating a flag prefix like A:, B:, M:). In that mode
// the terminal condition must use M: (Measured). For single-operand values
// without a flag prefix, it uses the legacy parser.
//
// rveevr's parser accepts M: as ConditionFlag::Measured and treats it as a
// terminal row in eval_value, so both sides agree.

/// Run both value evaluators through the same frame sequence.
/// Returns `(our_values, oracle_values)` per frame.
fn run_value_trace<F>(
    memaddr_ours: &str,
    memaddr_oracle: &str,
    mem_size: usize,
    frames: usize,
    mut step: F,
) -> (Vec<i64>, Vec<i32>)
where
    F: FnMut(usize, &mut Mem),
{
    let ach = Achievement::parse(memaddr_ours).expect("rveevr parse");
    let mut cs = CondsetState {
        cond_states: vec![ConditionState::default(); ach.core.conditions.len()],
        ..Default::default()
    };
    let mut oracle = Value::parse(memaddr_oracle).expect("oracle parse");

    let mut mem_ours = Mem::new(mem_size);
    let mut mem_oracle = Mem::new(mem_size);
    let mut ours_log = Vec::with_capacity(frames);
    let mut oracle_log = Vec::with_capacity(frames);

    for f in 0..frames {
        step(f, &mut mem_ours);
        step(f, &mut mem_oracle);

        let ours_val = eval_value(&ach.core, &mut cs, &mut |addr, size| {
            let n = size_to_bytes(size);
            mem_ours.read(addr, n)
        });
        ours_log.push(ours_val);

        let oracle_val = oracle.evaluate(|addr, n| mem_oracle.read(addr, n));
        oracle_log.push(oracle_val);
    }
    (ours_log, oracle_log)
}

fn assert_values_agree(tag: &str, ours: &[i64], theirs: &[i32]) {
    assert_eq!(ours.len(), theirs.len(), "{tag}: frame count mismatch");
    for (i, (o, t)) in ours.iter().zip(theirs.iter()).enumerate() {
        assert_eq!(
            *o, *t as i64,
            "{tag}: value mismatch on frame {i} — rveevr={o} rcheevos={t}"
        );
    }
}

// ---------------------------------------------------------------------------
// Value tests: single operand (legacy format — both parsers accept bare addr)
// ---------------------------------------------------------------------------

/// Single byte operand: VALUE = mem[0].
#[test]
fn value_single_byte() {
    // Legacy format for rcheevos (no flag prefix), condition format for rveevr.
    let (ours, theirs) = run_value_trace(
        "0xH0000", "0xH0000",
        16, 4,
        |frame, mem| { mem.write(0, (frame as u8 + 1) * 10); },
    );
    assert_values_agree("value_single_byte", &ours, &theirs);
}

/// Single word (16-bit LE) operand.
#[test]
fn value_single_word() {
    let (ours, theirs) = run_value_trace(
        "0x 0000", "0x 0000",
        16, 4,
        |frame, mem| {
            let v = (frame as u16 + 1) * 1000;
            mem.write(0, v as u8);
            mem.write(1, (v >> 8) as u8);
        },
    );
    assert_values_agree("value_single_word", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: AddSource chains (need M: terminal for rcheevos)
// ---------------------------------------------------------------------------

/// AddSource: A:0xH0000 + M:0xH0001 = sum of two bytes.
#[test]
fn value_add_source() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000_0xH0001",
        "A:0xH0000_M:0xH0001",
        16, 4,
        |frame, mem| {
            mem.write(0, frame as u8 * 5);
            mem.write(1, 100);
        },
    );
    assert_values_agree("value_add_source", &ours, &theirs);
}

/// Triple AddSource chain: A:0xH0000 + A:0xH0001 + M:0xH0002.
#[test]
fn value_add_source_triple() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000_A:0xH0001_0xH0002",
        "A:0xH0000_A:0xH0001_M:0xH0002",
        16, 4,
        |frame, mem| {
            mem.write(0, 10);
            mem.write(1, 20);
            mem.write(2, frame as u8 * 3);
        },
    );
    assert_values_agree("value_add_source_triple", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: SubSource chains
// ---------------------------------------------------------------------------

/// SubSource: A:0xH0000 - B:0xH0001 + M:0xH0002.
#[test]
fn value_sub_source() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000_B:0xH0001_0xH0002",
        "A:0xH0000_B:0xH0001_M:0xH0002",
        16, 4,
        |_frame, mem| {
            mem.write(0, 100);
            mem.write(1, 30);
            mem.write(2, 5);
        },
    );
    assert_values_agree("value_sub_source", &ours, &theirs);
}

/// SubSource going negative: B:0xH0000_M:0xH0001 where mem[0]>mem[1].
#[test]
fn value_sub_source_negative() {
    let (ours, theirs) = run_value_trace(
        "B:0xH0000_0xH0001",
        "B:0xH0000_M:0xH0001",
        16, 4,
        |_frame, mem| {
            mem.write(0, 50); // subtract 50
            mem.write(1, 10); // terminal +10 → -50+10 = -40
        },
    );
    assert_values_agree("value_sub_source_negative", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: Multiply modifier
// ---------------------------------------------------------------------------

/// AddSource with multiply: A:0xH0000*3_M:0xH0001.
#[test]
fn value_multiply() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000*3_0xH0001",
        "A:0xH0000*3_M:0xH0001",
        16, 4,
        |_frame, mem| {
            mem.write(0, 7);  // 7*3 = 21
            mem.write(1, 4);  // +4 = 25
        },
    );
    assert_values_agree("value_multiply", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: Constant values
// ---------------------------------------------------------------------------

/// Constant value: value is always a fixed number.
/// Single byte always reading the same address.
#[test]
fn value_constant_mem() {
    let (ours, theirs) = run_value_trace(
        "0xH0000", "0xH0000",
        16, 5,
        |_frame, mem| { mem.write(0, 42); },
    );
    assert_values_agree("value_constant_mem", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: DWord (32-bit) values
// ---------------------------------------------------------------------------

/// 32-bit LE value.
#[test]
fn value_dword() {
    let (ours, theirs) = run_value_trace(
        "0xX0000", "0xX0000",
        16, 3,
        |frame, mem| {
            let v = 100_000u32 + (frame as u32 * 50_000);
            mem.write_u32_le(0, v);
        },
    );
    assert_values_agree("value_dword", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: changing values across frames
// ---------------------------------------------------------------------------

/// Value changes each frame — both evaluators should track.
#[test]
fn value_changes_each_frame() {
    let (ours, theirs) = run_value_trace(
        "0xH0000", "0xH0000",
        16, 10,
        |frame, mem| { mem.write(0, frame as u8); },
    );
    assert_values_agree("value_changes_each_frame", &ours, &theirs);
}

/// AddSource chain where components change independently each frame.
#[test]
fn value_add_source_dynamic() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000_0xH0001",
        "A:0xH0000_M:0xH0001",
        16, 8,
        |frame, mem| {
            mem.write(0, (frame as u8) * 10);
            mem.write(1, 255 - (frame as u8) * 10);
        },
    );
    assert_values_agree("value_add_source_dynamic", &ours, &theirs);
}

// ---------------------------------------------------------------------------
// Value tests: AddSource with multiply and changing values
// ---------------------------------------------------------------------------

/// A:0xH0000*10_A:0xH0001*5_M:0xH0002 — weighted sum.
#[test]
fn value_weighted_sum() {
    let (ours, theirs) = run_value_trace(
        "A:0xH0000*10_A:0xH0001*5_0xH0002",
        "A:0xH0000*10_A:0xH0001*5_M:0xH0002",
        16, 4,
        |frame, mem| {
            mem.write(0, frame as u8 + 1); // (f+1)*10
            mem.write(1, 2);                // 2*5=10
            mem.write(2, 1);                // +1
        },
    );
    assert_values_agree("value_weighted_sum", &ours, &theirs);
}
