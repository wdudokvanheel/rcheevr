//! ROM integration harness for rcheevr.
//!
//! Boots a real Game Boy ROM in the emulator, wires up `AchievementRuntime`
//! with a `MockTransport` serving crafted achievement conditions, and verifies
//! that achievements trigger (or don't) based on actual emulator memory state.
//!
//! Set `TEST_ROM_GB` to the path of a `.gb` ROM to run these tests:
//!
//! ```text
//! TEST_ROM_GB=../gameboy-emulator/resources/roms/test/pokemon-red.gb \
//!     cargo test --test rom_harness -- --nocapture
//! ```
//!
//! Tests are skipped (pass with a log message) when `TEST_ROM_GB` is unset.

use gameboy_emulator::gameboy::{detect_device_model, GameBoy};
use rcheevr::client::AchievementRuntime;
use rcheevr::events::AchievementEvent;
use rcheevr::http::MockTransport;
use rcheevr::memaddr::MemSize;
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load the ROM at `$TEST_ROM_GB`. Returns `None` when the env var is unset.
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

/// Build a `MockTransport` pre-seeded with the login / gameid / patch /
/// startsession flow.  After those 4 expectations are consumed any further
/// calls (awardachievement, ping) return empty bytes — the runtime handles
/// that gracefully.
fn mock_with_patch(
    achievements: serde_json::Value,
    leaderboards: serde_json::Value,
    rich_presence: &str,
) -> MockTransport {
    let mut mock = MockTransport::new();
    mock.strict = false;
    let award_ids: Vec<u64> = achievements
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("ID").and_then(serde_json::Value::as_u64))
                .collect()
        })
        .unwrap_or_default();

    mock.expect_json(
        "",
        &[("r", "login2")],
        json!({
            "Success": true,
            "User": "tester",
            "Token": "TOKEN",
            "DisplayName": "tester",
            "Score": 0,
            "SoftcoreScore": 0,
            "Messages": 0,
            "Permissions": 0
        }),
    );

    mock.expect_json(
        "",
        &[("r", "gameid")],
        json!({
            "Success": true,
            "GameID": 1
        }),
    );

    mock.expect_json(
        "",
        &[("r", "patch")],
        json!({
            "Success": true,
            "Title": "Harness ROM",
            "ImageIcon": "",
            "ConsoleID": 4,
            "RichPresencePatch": rich_presence,
            "Achievements": achievements,
            "Leaderboards": leaderboards
        }),
    );

    mock.expect_json(
        "",
        &[("r", "startsession")],
        json!({
            "Success": true,
            "HardcoreUnlocks": [],
            "Unlocks": [],
            "ServerNow": 0
        }),
    );

    for id in award_ids {
        mock.expect_json(
            "",
            &[("r", "awardachievement")],
            json!({
                "Success": true,
                "Score": 0,
                "SoftcoreScore": 0,
                "AchievementID": id
            }),
        );
    }

    mock
}

/// Shorthand: mock with achievements only, no leaderboards.
fn mock_with_achievements(achievements: serde_json::Value) -> MockTransport {
    mock_with_patch(achievements, json!([]), "Display:\nPlaying")
}

fn runtime_with(mock: MockTransport) -> AchievementRuntime<MockTransport> {
    AchievementRuntime::with_base_url(mock, "http://mock".to_string())
}

fn boot_gb(rom: &[u8]) -> GameBoy {
    let model = detect_device_model(rom);
    let mut gb = GameBoy::new(model);
    gb.load_rom(rom);
    gb
}

/// Peek closure that wires the emulator bus into rcheevr's address space.
/// Identical to the one used in portacore-desktop / portacore-wasm.
fn peek_bus(gb: &GameBoy, addr: u32, size: MemSize) -> u32 {
    let bytes: u32 = match size {
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
    };
    let mut val: u32 = 0;
    for i in 0..bytes {
        let a = addr.wrapping_add(i) as u16;
        val |= (gb.bus.peek(a) as u32) << (8 * i);
    }
    val
}

/// Run `n` frames, collecting all events.
async fn run_frames(
    gb: &mut GameBoy,
    rt: &mut AchievementRuntime<MockTransport>,
    n: usize,
) -> Vec<AchievementEvent> {
    let mut all = Vec::new();
    for _ in 0..n {
        gb.run_frame();
        rt.peek_from(|addr, size| peek_bus(gb, addr, size)).await;
        all.extend(rt.drain_events());
    }
    all
}

/// Extract Unlocked achievement IDs from an event list.
fn unlocked_ids(events: &[AchievementEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            AchievementEvent::Unlocked { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Achievement JSON helpers
// ---------------------------------------------------------------------------

fn ach(id: u32, title: &str, memaddr: &str, points: u32) -> serde_json::Value {
    json!({
        "ID": id,
        "Title": title,
        "Description": "",
        "Points": points,
        "MemAddr": memaddr,
        "Flags": 3,
        "Author": "harness",
        "BadgeName": "00001"
    })
}

// ===========================================================================
// Tests
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. ROM header constant triggers with hit count
//
// Address 0x0104 is the first byte of the Nintendo logo (0xCE = 206).
// A hit count of 5 means the achievement exits WAITING on frame 1
// (condition true but target not reached) and fires on frame 5.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rom_header_hit_count() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1001, "ROM Header", "0xH0104=206(5)", 5)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);

    // First 4 frames — should not fire.
    let early = run_frames(&mut gb, &mut rt, 4).await;
    assert!(
        unlocked_ids(&early).is_empty(),
        "should not fire before frame 5"
    );

    // Frame 5 — fires.
    let frame5 = run_frames(&mut gb, &mut rt, 1).await;
    assert_eq!(unlocked_ids(&frame5), vec![1001]);
}

// ---------------------------------------------------------------------------
// 2. Hit-count condition delays trigger
//
// 0xH0104=206(50) — ROM header byte is always 0xCE. With the peek fix
// (no DMA/VRAM blocking), this is true every frame. Hit target 50 fires
// on exactly frame 50.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hit_count_delays_trigger() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1002, "Fifty Frames", "0xH0104=206(50)", 10)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);

    // 49 frames — should not fire yet.
    let early = run_frames(&mut gb, &mut rt, 49).await;
    assert!(
        unlocked_ids(&early).is_empty(),
        "should not trigger before frame 50"
    );

    // Frame 50 — fires.
    let frame50 = run_frames(&mut gb, &mut rt, 1).await;
    assert_eq!(
        unlocked_ids(&frame50),
        vec![1002],
        "should trigger on frame 50"
    );
}

// ---------------------------------------------------------------------------
// 3. Impossible condition never fires
//
// 0xH0104=0 — the Nintendo logo byte is 0xCE, never 0.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn impossible_condition_never_fires() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1003, "Impossible", "0xH0104=0", 50)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);
    let events = run_frames(&mut gb, &mut rt, 300).await;

    assert!(
        unlocked_ids(&events).is_empty(),
        "impossible condition must never fire"
    );
}

// ---------------------------------------------------------------------------
// 4. Emulator WRAM write detected
//
// 0xHC001>0 — second byte of WRAM. On boot, WRAM is zeroed. Pokemon Red
// writes to 0xC001 within the first ~100 frames during initialisation.
// We run 600 frames to give any commercial ROM plenty of time.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wram_write_detected() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1004, "WRAM Init", "0xHC001>0", 5)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);
    let events = run_frames(&mut gb, &mut rt, 600).await;

    assert_eq!(
        unlocked_ids(&events),
        vec![1004],
        "game should write to WRAM within 600 frames"
    );
}

// ---------------------------------------------------------------------------
// 5. Delta detection — memory value changes between frames
//
// d0xHC001!=0xHC001 — fires when the previous frame's value at 0xC001
// differs from the current frame's value. Once the game writes a non-zero
// value the condition triggers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delta_detects_memory_change() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1005, "Memory Changed", "d0xHC001!=0xHC001", 5)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);
    let events = run_frames(&mut gb, &mut rt, 600).await;

    assert!(
        !unlocked_ids(&events).is_empty(),
        "delta condition should fire once the game writes to 0xC001"
    );
}

// ---------------------------------------------------------------------------
// 6. Multiple achievements with staggered triggers
//
// Two achievements with different hit counts on the ROM header constant.
// Hit-count achievements exit WAITING on frame 1 (condition true but
// target not reached), then trigger when their hit count is met.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_achievements_staggered() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([
        ach(1006, "Quick", "0xH0104=206(10)", 5),
        ach(1007, "Delayed", "0xH0104=206(60)", 10)
    ]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);

    // First 9 frames — neither should fire.
    let early = run_frames(&mut gb, &mut rt, 9).await;
    assert!(
        unlocked_ids(&early).is_empty(),
        "neither should fire before frame 10"
    );

    // Frame 10 — quick fires.
    let frame10 = run_frames(&mut gb, &mut rt, 1).await;
    assert_eq!(unlocked_ids(&frame10), vec![1006]);

    // Frames 11..59 — delayed should not fire yet.
    let mid = run_frames(&mut gb, &mut rt, 49).await;
    assert!(
        unlocked_ids(&mid).is_empty(),
        "delayed should not fire before frame 60"
    );

    // Frame 60 — delayed fires.
    let frame60 = run_frames(&mut gb, &mut rt, 1).await;
    assert_eq!(
        unlocked_ids(&frame60),
        vec![1007],
        "delayed should trigger on frame 60"
    );
}

// ---------------------------------------------------------------------------
// 7. No re-award on subsequent frames
//
// After an achievement fires, it must not fire again even if the condition
// remains true. Uses WRAM condition that triggers once during init.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_re_award() {
    let rom = require_rom!();

    let mock = mock_with_achievements(json!([ach(1008, "Once Only", "0xHC001>0", 5)]));

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);

    // Run enough frames for the achievement to trigger (WRAM written by ~frame 100).
    let first = run_frames(&mut gb, &mut rt, 600).await;
    assert_eq!(unlocked_ids(&first), vec![1008]);

    // 200 more frames — must not re-award.
    let rest = run_frames(&mut gb, &mut rt, 200).await;
    assert!(
        unlocked_ids(&rest).is_empty(),
        "achievement must not re-award after initial trigger"
    );
}

// ---------------------------------------------------------------------------
// 8. Rich presence renders
//
// A simple RP script that reads a memory value. After enough frames the
// rendered string should be non-empty.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rich_presence_renders() {
    let rom = require_rom!();

    let rp = "Display:\nPlaying Test ROM";
    let mock = mock_with_patch(json!([]), json!([]), rp);

    let mut rt = runtime_with(mock);
    rt.login_password("tester", "pw").await.unwrap();
    rt.load_rom(&rom).await.unwrap();

    let mut gb = boot_gb(&rom);
    let _ = run_frames(&mut gb, &mut rt, 10).await;

    assert_eq!(rt.rich_presence(), "Playing Test ROM");
}

// ---------------------------------------------------------------------------
// 9. ROM hash is deterministic
//
// hash_gb on the same ROM data always returns the same 32-char hex string.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rom_hash_deterministic() {
    let rom = require_rom!();

    let h1 = rcheevr::hash::hash_gb(&rom);
    let h2 = rcheevr::hash::hash_gb(&rom);

    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 32, "MD5 hex string should be 32 chars");
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit()), "should be hex");
}
