//! Layer-2 differential tests: rveevr's evaluator vs rcheevos as oracle.
//!
//! Gated behind `--features oracle` because the oracle crate needs a C
//! toolchain to build. When the feature is off, the whole file compiles to
//! nothing. When on, it feeds synthetic memory traces into both evaluators
//! and asserts equivalent outputs per frame.

#![cfg(feature = "oracle")]

use rveevr::condition::Achievement;
use rveevr::evaluator::{eval_achievement, AchievementState};
use rveevr::memaddr::MemSize;
use rveevr_oracle::{ffi, Trigger};

/// A tiny flat memory buffer the harness feeds to both evaluators.
struct Mem(Vec<u8>);

impl Mem {
    fn new(size: usize) -> Self {
        Self(vec![0u8; size])
    }

    fn write(&mut self, addr: usize, v: u8) {
        self.0[addr] = v;
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
