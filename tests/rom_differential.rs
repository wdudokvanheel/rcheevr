//! ROM differential tests: rveevr vs rcheevos on a real Game Boy ROM.
//!
//! Boots a real ROM, feeds the same `bus.peek` memory to both evaluators
//! each frame, and asserts they trigger on the same frame.
//!
//! Requires both the `oracle` feature (C toolchain) and `TEST_ROM_GB`:
//!
//! ```text
//! TEST_ROM_GB=path/to/rom.gb cargo test --test rom_differential --features oracle -- --nocapture
//! ```
//!
//! Tests are skipped when either is missing.

#![cfg(feature = "oracle")]

use gameboy_emulator::gameboy::{detect_device_model, Button, GameBoy};
use rveevr::condition::Achievement;
use rveevr::evaluator::{eval_achievement, AchievementState, CondsetState, ConditionState};
use rveevr::leaderboard::eval_value;
use rveevr::memaddr::MemSize;
use rveevr_oracle::{ffi, Trigger, Value};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_test_rom() -> Option<Vec<u8>> {
    let path = std::env::var("TEST_ROM_GB").ok()?;
    Some(std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}")))
}

macro_rules! require_rom {
    () => {
        match load_test_rom() {
            Some(r) => r,
            None => {
                eprintln!("TEST_ROM_GB not set — skipping");
                return;
            }
        }
    };
}

fn boot_gb(rom: &[u8]) -> GameBoy {
    let model = detect_device_model(rom);
    let mut gb = GameBoy::new(model);
    gb.load_rom(rom);
    gb
}

/// Translate rveevr MemSize to byte count for the rcheevos peek callback.
fn size_to_bytes(size: MemSize) -> u32 {
    match size {
        MemSize::Byte
        | MemSize::LowNibble
        | MemSize::HighNibble
        | MemSize::BitCount
        | MemSize::Bit0
        | MemSize::Bit1
        | MemSize::Bit2
        | MemSize::Bit3
        | MemSize::Bit4
        | MemSize::Bit5
        | MemSize::Bit6
        | MemSize::Bit7 => 1,
        MemSize::Word | MemSize::BigEndianWord => 2,
        MemSize::ThreeBytes | MemSize::BigEndianThreeBytes => 3,
        _ => 4,
    }
}

/// Read `num_bytes` little-endian from the GB bus (for rcheevos peek).
fn peek_raw(gb: &GameBoy, addr: u32, num_bytes: u32) -> u32 {
    let mut val: u32 = 0;
    for i in 0..num_bytes {
        let a = addr.wrapping_add(i) as u16;
        val |= (gb.bus.peek(a) as u32) << (8 * i);
    }
    val
}

/// Read from the GB bus using rveevr's MemSize (for rveevr peek).
fn peek_sized(gb: &GameBoy, addr: u32, size: MemSize) -> u32 {
    peek_raw(gb, addr, size_to_bytes(size))
}

/// Run both evaluators over `frames` frames on the same ROM.
/// Returns `(rveevr_triggered, rcheevos_triggered)` per frame.
fn run_rom_trace(
    gb: &mut GameBoy,
    memaddr: &str,
    frames: usize,
) -> (Vec<bool>, Vec<bool>) {
    let ach = Achievement::parse(memaddr).unwrap_or_else(|e| {
        panic!("rveevr parse failed for {memaddr:?}: {e:?}")
    });
    let mut ours_state = AchievementState::for_achievement(&ach);
    let mut theirs = Trigger::parse(memaddr).unwrap_or_else(|e| {
        panic!("rcheevos parse failed for {memaddr:?}: {e:?}")
    });

    let mut ours_log = Vec::with_capacity(frames);
    let mut theirs_log = Vec::with_capacity(frames);

    for _ in 0..frames {
        gb.run_frame();

        let ours_triggered = eval_achievement(&ach, &mut ours_state, |addr, size| {
            peek_sized(gb, addr, size)
        });
        ours_log.push(ours_triggered);

        let state = theirs.evaluate(|addr, n| peek_raw(gb, addr, n));
        theirs_log.push(state == ffi::RC_TRIGGER_STATE_TRIGGERED);
    }

    (ours_log, theirs_log)
}

fn assert_agree(tag: &str, ours: &[bool], theirs: &[bool]) {
    let ours_first = ours.iter().position(|b| *b);
    let theirs_first = theirs.iter().position(|b| *b);
    assert_eq!(
        ours_first, theirs_first,
        "{tag}: first-trigger frame mismatch — rveevr={ours_first:?} rcheevos={theirs_first:?}"
    );
}

// ---------------------------------------------------------------------------
// Test cases — crafted conditions exercising different evaluator features
// against real ROM memory.
//
// NOTE: rcheevos starts triggers in a WAITING state that prevents firing
// when conditions are trivially true from frame 0 (anti-auto-trigger).
// rveevr's low-level evaluator does not implement this state machine —
// it fires as soon as conditions are met. All tests below therefore use
// conditions that start FALSE and become true during gameplay, matching
// how real achievements are authored.
// ---------------------------------------------------------------------------

/// WAITING state: a condition that is always true from boot never fires.
/// Both evaluators should agree: neither triggers in 100 frames.
#[test]
fn diff_waiting_blocks_always_true() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0xH0104=206", 100);
    assert_agree("waiting_blocks_always_true", &ours, &theirs);
    assert!(ours.iter().all(|b| !b), "should never trigger when always true from boot");
}

/// Byte not-equal comparison on WRAM.
/// 0xC001 != 0 fires once the game writes any non-zero value.
#[test]
fn diff_byte_not_equal() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0xHC001!=0", 600);
    assert_agree("byte_not_equal", &ours, &theirs);
}

/// Hit count on a constant-true condition.
/// ROM header byte is always 0xCE — hit target 30 fires on frame 30.
#[test]
fn diff_hit_count() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0xH0104=206(30)", 60);
    assert_agree("hit_count", &ours, &theirs);
    let first = ours.iter().position(|b| *b);
    assert_eq!(first, Some(29), "should trigger on frame 30 (index 29)");
}

/// Impossible condition — never fires for either evaluator.
#[test]
fn diff_impossible() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0xH0104=0", 100);
    assert_agree("impossible", &ours, &theirs);
    assert!(ours.iter().all(|b| !b), "should never trigger");
}

/// Greater-than comparison on WRAM.
/// 0xC001 > 0 fires once the game writes a non-zero value.
#[test]
fn diff_wram_greater() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0xHC001>0", 600);
    assert_agree("wram_greater", &ours, &theirs);
}

/// Delta detection — fires when 0xC001 changes between frames.
#[test]
fn diff_delta() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "d0xHC001!=0xHC001", 600);
    assert_agree("delta", &ours, &theirs);
}

/// Word (16-bit) comparison on WRAM.
/// 16-bit LE at C001 (spanning C001-C002) starts at 0, becomes non-zero
/// once the game writes to WRAM during init.
#[test]
fn diff_word_wram() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(&mut gb, "0x C001>0", 600);
    assert_agree("word_wram", &ours, &theirs);
}

/// Multi-condset: core + alt.
/// Core: ROM header byte = 0xCE (always true).
/// Alt1: WRAM 0xC001 > 0 (fires once game writes).
/// Alt2: impossible (0xH0104=0).
/// Trigger when core && (alt1 || alt2).
#[test]
fn diff_multi_condset() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "0xH0104=206S0xHC001>0S0xH0104=0",
        600,
    );
    assert_agree("multi_condset", &ours, &theirs);
}

/// ResetIf: achievement resets when 0xC001 = 0, only fires when 0xC001 > 0
/// with hit count. Because ResetIf clears hits each frame 0xC001=0, the
/// hit count only starts accumulating once the game writes to WRAM.
#[test]
fn diff_resetif() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "0xH0104=206(10)_R:0xHC001=0",
        600,
    );
    assert_agree("resetif", &ours, &theirs);
}

/// PauseIf: evaluation pauses while 0xC001 = 0. Once WRAM is written,
/// the pause lifts and the hit count begins accumulating on the ROM header.
#[test]
fn diff_pauseif() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "0xH0104=206(10)_P:0xHC001=0",
        600,
    );
    assert_agree("pauseif", &ours, &theirs);
}

/// AndNext: two conditions chained. Both must be true on the same frame.
/// 0xH0104=206 (always true) AndNext 0xHC001>0 (fires once WRAM written).
#[test]
fn diff_andnext() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "N:0xH0104=206_0xHC001>0",
        600,
    );
    assert_agree("andnext", &ours, &theirs);
}

/// OrNext: either condition can satisfy the group.
/// 0xH0104=0 (always false) OrNext 0xHC001>0 (true once WRAM written).
/// Initially false||false = false, fires once game writes to 0xC001.
#[test]
fn diff_ornext() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "O:0xH0104=0_0xHC001>0",
        600,
    );
    assert_agree("ornext", &ours, &theirs);
    assert!(ours.iter().any(|b| *b), "OrNext should trigger");
}

/// AddSource: adds value at one address to the next comparison.
/// AddSource 0xHC001 + 0xHC002 > 0. Both WRAM bytes start at 0 (sum=0),
/// fires once the game writes a non-zero value to either address.
#[test]
fn diff_addsource() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "A:0xHC001_0xHC002>0",
        600,
    );
    assert_agree("addsource", &ours, &theirs);
    assert!(ours.iter().any(|b| *b), "AddSource should trigger");
}

/// Trigger flag: condition with Trigger flag marks the achievement as
/// "primed" until the trigger condition fires. Here we use a trigger flag
/// on the WRAM write detection.
#[test]
fn diff_trigger_flag() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "0xH0104=206_T:0xHC001>0",
        600,
    );
    assert_agree("trigger_flag", &ours, &theirs);
}

/// Remember/Recall: store WRAM value then compare via {recall}.
/// K:0xHC001 remembers the value at C001, {recall}>0 fires once it's non-zero.
#[test]
fn diff_remember_recall() {
    let rom = require_rom!();
    let mut gb = boot_gb(&rom);
    let (ours, theirs) = run_rom_trace(
        &mut gb,
        "K:0xHC001_{recall}>0",
        600,
    );
    assert_agree("remember_recall", &ours, &theirs);
    assert!(ours.iter().any(|b| *b), "Remember/Recall should trigger");
}

// ===========================================================================
// Input helpers
// ===========================================================================

/// Press a button for `hold` frames, then release and wait `wait` frames.
/// Returns the total frames consumed.
fn press(gb: &mut GameBoy, button: Button, hold: usize, wait: usize) -> usize {
    gb.set_input(button, true);
    for _ in 0..hold {
        gb.run_frame();
    }
    gb.set_input(button, false);
    for _ in 0..wait {
        gb.run_frame();
    }
    hold + wait
}

/// Skip through Pokémon Red's title screen and intro into gameplay.
/// Mashes Start and A to get past all menus. Returns the number of
/// frames consumed (not counted toward the differential evaluation).
fn skip_pokemon_red_intro(gb: &mut GameBoy) -> usize {
    let mut total = 0;
    // Boot + Nintendo logo + title screen animation
    for _ in 0..300 { gb.run_frame(); total += 1; }
    // Press Start on title screen
    total += press(gb, Button::Start, 5, 60);
    // "NEW GAME" / "CONTINUE" menu — press A to select
    total += press(gb, Button::A, 5, 120);
    // Prof. Oak's intro dialogue — mash A through ~20 text boxes
    for _ in 0..40 {
        total += press(gb, Button::A, 3, 30);
    }
    // Name entry — press Start to accept default name, then A to confirm
    total += press(gb, Button::Start, 5, 30);
    total += press(gb, Button::A, 5, 60);
    // Rival name — same thing
    total += press(gb, Button::Start, 5, 30);
    total += press(gb, Button::A, 5, 60);
    // More Oak dialogue + shrinking animation
    for _ in 0..30 {
        total += press(gb, Button::A, 3, 30);
    }
    // Let the game settle into overworld
    for _ in 0..120 { gb.run_frame(); total += 1; }
    total
}

// ===========================================================================
// Bulk differential: real RA achievement data against a real ROM
// ===========================================================================
//
// Loads achievement definitions from fixture JSON, boots the actual ROM,
// runs every achievement and leaderboard VALUE through both evaluators
// for N frames, and asserts perfect agreement.

#[derive(Deserialize)]
struct PatchResponse {
    #[serde(rename = "PatchData")]
    patch_data: PatchData,
}

#[derive(Deserialize)]
struct PatchData {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Achievements")]
    achievements: Vec<AchievementDef>,
    #[serde(rename = "Leaderboards")]
    leaderboards: Vec<LeaderboardDef>,
}

#[derive(Deserialize)]
struct AchievementDef {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "MemAddr")]
    memaddr: String,
    #[serde(rename = "Flags")]
    flags: u32,
}

#[derive(Deserialize)]
struct LeaderboardDef {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Mem")]
    mem: String,
}

fn load_fixture(name: &str) -> PatchData {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
    let resp: PatchResponse =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("failed to parse {path}: {e}"));
    resp.patch_data
}

/// Run ALL core achievements from Pokémon Red's real RA data against the
/// actual ROM for many frames of real emulation.
///
/// Both evaluators see identical memory each frame. We assert:
/// 1. Both parse every achievement successfully.
/// 2. Both agree on triggered state every single frame.
///
/// Most achievements won't trigger during boot — that's fine. The test
/// verifies the evaluators walk in lockstep through complex real-world
/// condition chains with real memory patterns.
#[test]
fn diff_pokemon_red_all_achievements() {
    let rom = require_rom!();
    let pd = load_fixture("pokemon_red_patch.json");
    let core: Vec<_> = pd.achievements.iter().filter(|a| a.flags == 3).collect();
    assert!(!core.is_empty(), "no core achievements in fixture");

    let frames = 10_000;
    let mut gb = boot_gb(&rom);

    // Parse all achievements up front through both parsers.
    let mut pairs: Vec<(
        &AchievementDef,
        Achievement,
        AchievementState,
        Trigger,
    )> = Vec::new();

    let mut parse_failures = Vec::new();
    for a in &core {
        let ours = match Achievement::parse(&a.memaddr) {
            Ok(ach) => ach,
            Err(e) => {
                parse_failures.push(format!("[{}] {} rveevr: {:?}", a.id, a.title, e));
                continue;
            }
        };
        let theirs = match Trigger::parse(&a.memaddr) {
            Ok(t) => t,
            Err(e) => {
                parse_failures.push(format!("[{}] {} rcheevos: {:?}", a.id, a.title, e));
                continue;
            }
        };
        let state = AchievementState::for_achievement(&ours);
        pairs.push((a, ours, state, theirs));
    }

    if !parse_failures.is_empty() {
        panic!(
            "{} parse failures out of {} core achievements:\n{}",
            parse_failures.len(),
            core.len(),
            parse_failures.join("\n")
        );
    }

    eprintln!(
        "Running {} core achievements × {} frames on {}...",
        pairs.len(), frames, pd.title
    );

    // Run frames, evaluating every achievement through both evaluators.
    let mut disagreements: Vec<String> = Vec::new();

    for frame in 0..frames {
        gb.run_frame();

        for (def, ach, ach_state, oracle_trigger) in pairs.iter_mut() {
            let ours_triggered = eval_achievement(ach, ach_state, |addr, size| {
                peek_sized(&gb, addr, size)
            });

            let oracle_state = oracle_trigger.evaluate(|addr, n| peek_raw(&gb, addr, n));
            let theirs_triggered = oracle_state == ffi::RC_TRIGGER_STATE_TRIGGERED;

            if ours_triggered != theirs_triggered {
                disagreements.push(format!(
                    "frame {}: [{}] {} — rveevr={} rcheevos={} (state={})",
                    frame, def.id, def.title, ours_triggered, theirs_triggered, oracle_state
                ));
            }
        }
    }

    // Report.
    let triggered_ours: Vec<_> = pairs.iter()
        .filter(|(_, _, s, _)| s.triggered)
        .map(|(d, _, _, _)| format!("[{}] {}", d.id, d.title))
        .collect();

    eprintln!("Achievements that triggered during {} frames: {}", frames, triggered_ours.len());
    for t in &triggered_ours {
        eprintln!("  {t}");
    }

    assert!(
        disagreements.is_empty(),
        "{} disagreements across {} achievements × {} frames:\n{}",
        disagreements.len(),
        pairs.len(),
        frames,
        disagreements.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );

    eprintln!(
        "PASS: {} achievements × {} frames = {} evaluations, zero disagreements",
        pairs.len(), frames, pairs.len() * frames
    );
}

/// Run leaderboard VALUE expressions from Pokémon Red through both evaluators.
///
/// For each leaderboard, parse the VAL section and run rveevr's eval_value
/// vs rcheevos' rc_evaluate_value for 1000 frames.
#[test]
fn diff_pokemon_red_leaderboard_values() {
    let rom = require_rom!();
    let pd = load_fixture("pokemon_red_patch.json");
    assert!(!pd.leaderboards.is_empty(), "no leaderboards in fixture");

    let frames = 10_000;
    let mut gb = boot_gb(&rom);

    // For each leaderboard, extract the VAL section.
    struct LbPair<'a> {
        def: &'a LeaderboardDef,
        val_condset: rveevr::condition::Condset,
        val_state: CondsetState,
        oracle: Value,
    }

    let mut pairs: Vec<LbPair> = Vec::new();
    let mut parse_failures = Vec::new();

    for lb in &pd.leaderboards {
        // Extract VAL section from the mem string.
        let val_str = lb.mem.split("::")
            .find(|p| p.starts_with("VAL:"))
            .map(|p| &p[4..]);
        let val_str = match val_str {
            Some(s) => s,
            None => {
                parse_failures.push(format!("[{}] {}: no VAL: section", lb.id, lb.title));
                continue;
            }
        };

        // Parse through rveevr.
        let ach = match Achievement::parse(val_str) {
            Ok(a) => a,
            Err(e) => {
                parse_failures.push(format!("[{}] {} rveevr: {:?}\n  val: {}", lb.id, lb.title, e, val_str));
                continue;
            }
        };
        let cs = CondsetState {
            cond_states: vec![ConditionState::default(); ach.core.conditions.len()],
            ..Default::default()
        };

        // Parse through rcheevos. rcheevos' rc_parse_value accepts legacy
        // format (bare operands with multipliers) without needing M: prefix.
        let oracle = match Value::parse(val_str) {
            Ok(v) => v,
            Err(e) => {
                parse_failures.push(format!("[{}] {} rcheevos: {:?}\n  val: {}", lb.id, lb.title, e, val_str));
                continue;
            }
        };

        pairs.push(LbPair {
            def: lb,
            val_condset: ach.core,
            val_state: cs,
            oracle,
        });
    }

    if !parse_failures.is_empty() {
        panic!(
            "{} leaderboard parse failures:\n{}",
            parse_failures.len(),
            parse_failures.join("\n")
        );
    }

    eprintln!(
        "Running {} leaderboard VALUEs × {} frames on {}...",
        pairs.len(), frames, pd.title
    );

    let mut disagreements: Vec<String> = Vec::new();

    for frame in 0..frames {
        gb.run_frame();

        for pair in pairs.iter_mut() {
            let ours_val = eval_value(
                &pair.val_condset,
                &mut pair.val_state,
                &mut |addr, size| peek_sized(&gb, addr, size),
            );

            let theirs_val = pair.oracle.evaluate(|addr, n| peek_raw(&gb, addr, n)) as i64;

            if ours_val != theirs_val {
                disagreements.push(format!(
                    "frame {}: LB[{}] {} — rveevr={} rcheevos={}",
                    frame, pair.def.id, pair.def.title, ours_val, theirs_val
                ));
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} value disagreements across {} leaderboards × {} frames:\n{}",
        disagreements.len(),
        pairs.len(),
        frames,
        disagreements.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );

    eprintln!(
        "PASS: {} leaderboard VALUEs × {} frames = {} evaluations, zero disagreements",
        pairs.len(), frames, pairs.len() * frames
    );
}

// ===========================================================================
// Gameplay-driven differential: skip intro, then play with real inputs
// ===========================================================================

/// Run all 91 Pokémon Red achievements + 3 leaderboard values through both
/// evaluators during actual gameplay (past the intro, walking around, entering
/// menus, etc.). 60K frames ≈ 17 minutes of game time.
///
/// This is the highest-fidelity differential test: real ROM, real achievement
/// data, real memory mutations from gameplay.
#[test]
fn diff_pokemon_red_gameplay() {
    let rom = require_rom!();
    let pd = load_fixture("pokemon_red_patch.json");
    let core: Vec<_> = pd.achievements.iter().filter(|a| a.flags == 3).collect();
    assert!(!core.is_empty(), "no core achievements in fixture");

    let eval_frames = 60_000;
    let mut gb = boot_gb(&rom);

    // Skip through the entire intro without evaluating achievements.
    let intro_frames = skip_pokemon_red_intro(&mut gb);
    eprintln!("Skipped intro: {intro_frames} frames consumed");

    // --- Parse all achievements ---
    let mut ach_pairs: Vec<(
        &AchievementDef,
        Achievement,
        AchievementState,
        Trigger,
    )> = Vec::new();

    for a in &core {
        let ours = Achievement::parse(&a.memaddr)
            .unwrap_or_else(|e| panic!("[{}] {} rveevr parse: {:?}", a.id, a.title, e));
        let theirs = Trigger::parse(&a.memaddr)
            .unwrap_or_else(|e| panic!("[{}] {} rcheevos parse: {:?}", a.id, a.title, e));
        let state = AchievementState::for_achievement(&ours);
        ach_pairs.push((a, ours, state, theirs));
    }

    // --- Parse all leaderboard VAL sections ---
    struct LbPair2<'a> {
        def: &'a LeaderboardDef,
        val_condset: rveevr::condition::Condset,
        val_state: CondsetState,
        oracle: Value,
    }

    let mut lb_pairs: Vec<LbPair2> = Vec::new();
    for lb in &pd.leaderboards {
        let val_str = lb.mem.split("::")
            .find(|p| p.starts_with("VAL:"))
            .map(|p| &p[4..]);
        let val_str = match val_str {
            Some(s) => s,
            None => continue,
        };
        let ach = Achievement::parse(val_str)
            .unwrap_or_else(|e| panic!("LB[{}] {} rveevr: {:?}", lb.id, lb.title, e));
        let cs = CondsetState {
            cond_states: vec![ConditionState::default(); ach.core.conditions.len()],
            ..Default::default()
        };
        let oracle = Value::parse(val_str)
            .unwrap_or_else(|e| panic!("LB[{}] {} rcheevos: {:?}", lb.id, lb.title, e));
        lb_pairs.push(LbPair2 {
            def: lb,
            val_condset: ach.core,
            val_state: cs,
            oracle,
        });
    }

    eprintln!(
        "Gameplay differential: {} achievements + {} leaderboards × {} frames on {}",
        ach_pairs.len(), lb_pairs.len(), eval_frames, pd.title
    );

    let mut ach_disagreements: Vec<String> = Vec::new();
    let mut val_disagreements: Vec<String> = Vec::new();

    // Schedule some gameplay inputs to create interesting memory patterns.
    // These fire at specific frame offsets during the evaluation window.
    // Frame → (button, press=true/release=false)
    let mut scheduled_inputs: Vec<(usize, Button, bool)> = Vec::new();

    // Walk around in the overworld: alternate directions
    let mut f = 0;
    for _ in 0..50 {
        // Walk down
        scheduled_inputs.push((f, Button::Down, true));
        scheduled_inputs.push((f + 30, Button::Down, false));
        f += 60;
        // Walk right
        scheduled_inputs.push((f, Button::Right, true));
        scheduled_inputs.push((f + 30, Button::Right, false));
        f += 60;
        // Walk up
        scheduled_inputs.push((f, Button::Up, true));
        scheduled_inputs.push((f + 30, Button::Up, false));
        f += 60;
        // Walk left
        scheduled_inputs.push((f, Button::Left, true));
        scheduled_inputs.push((f + 30, Button::Left, false));
        f += 60;
    }
    // Open menu periodically
    for burst in 0..10 {
        let base = 12_000 + burst * 4_000;
        scheduled_inputs.push((base, Button::Start, true));
        scheduled_inputs.push((base + 5, Button::Start, false));
        // Navigate menu
        scheduled_inputs.push((base + 60, Button::Down, true));
        scheduled_inputs.push((base + 65, Button::Down, false));
        scheduled_inputs.push((base + 100, Button::A, true));
        scheduled_inputs.push((base + 105, Button::A, false));
        // Close menu
        scheduled_inputs.push((base + 300, Button::B, true));
        scheduled_inputs.push((base + 305, Button::B, false));
        scheduled_inputs.push((base + 400, Button::B, true));
        scheduled_inputs.push((base + 405, Button::B, false));
    }

    // Sort by frame so we can scan linearly.
    scheduled_inputs.sort_by_key(|&(f, _, _)| f);
    let mut input_idx = 0;

    for frame in 0..eval_frames {
        // Apply any scheduled inputs for this frame.
        while input_idx < scheduled_inputs.len() && scheduled_inputs[input_idx].0 == frame {
            let (_, button, pressed) = scheduled_inputs[input_idx];
            gb.set_input(button, pressed);
            input_idx += 1;
        }

        gb.run_frame();

        // Evaluate achievements.
        for (def, ach, ach_state, oracle_trigger) in ach_pairs.iter_mut() {
            let ours_triggered = eval_achievement(ach, ach_state, |addr, size| {
                peek_sized(&gb, addr, size)
            });
            let oracle_state = oracle_trigger.evaluate(|addr, n| peek_raw(&gb, addr, n));
            let theirs_triggered = oracle_state == ffi::RC_TRIGGER_STATE_TRIGGERED;

            if ours_triggered != theirs_triggered {
                ach_disagreements.push(format!(
                    "frame {}: [{}] {} — rveevr={} rcheevos={} (state={})",
                    frame, def.id, def.title, ours_triggered, theirs_triggered, oracle_state
                ));
            }
        }

        // Evaluate leaderboard values.
        for pair in lb_pairs.iter_mut() {
            let ours_val = eval_value(
                &pair.val_condset,
                &mut pair.val_state,
                &mut |addr, size| peek_sized(&gb, addr, size),
            );
            let theirs_val = pair.oracle.evaluate(|addr, n| peek_raw(&gb, addr, n)) as i64;

            if ours_val != theirs_val {
                val_disagreements.push(format!(
                    "frame {}: LB[{}] {} — rveevr={} rcheevos={}",
                    frame, pair.def.id, pair.def.title, ours_val, theirs_val
                ));
            }
        }
    }

    // Report triggered achievements.
    let triggered: Vec<_> = ach_pairs.iter()
        .filter(|(_, _, s, _)| s.triggered)
        .map(|(d, _, _, _)| format!("[{}] {}", d.id, d.title))
        .collect();
    eprintln!("Achievements triggered during gameplay: {}", triggered.len());
    for t in &triggered {
        eprintln!("  {t}");
    }

    let total_ach_evals = ach_pairs.len() * eval_frames;
    let total_val_evals = lb_pairs.len() * eval_frames;
    let total = total_ach_evals + total_val_evals;

    assert!(
        ach_disagreements.is_empty(),
        "{} achievement disagreements across {} evals:\n{}",
        ach_disagreements.len(),
        total_ach_evals,
        ach_disagreements.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );

    assert!(
        val_disagreements.is_empty(),
        "{} value disagreements across {} evals:\n{}",
        val_disagreements.len(),
        total_val_evals,
        val_disagreements.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );

    eprintln!(
        "PASS: {} total evaluations ({} achievement + {} value) across {} gameplay frames, zero disagreements",
        total, total_ach_evals, total_val_evals, eval_frames
    );
}
