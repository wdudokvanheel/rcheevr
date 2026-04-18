//! Parse-compatibility tests against real RetroAchievements patch data.
//!
//! Feeds every achievement memaddr and leaderboard mem string from captured
//! fixture files through rveevr's parsers. No oracle needed — this tests
//! parser coverage against production achievement data.
//!
//! Fixture files live in `tests/fixtures/*.json` (RA patch endpoint responses).

use rveevr::condition::Achievement;
use rveevr::leaderboard::Leaderboard;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Minimal JSON types matching the RA patch response envelope
// ---------------------------------------------------------------------------

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
    let json =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
    let resp: PatchResponse =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("failed to parse {path}: {e}"));
    resp.patch_data
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Parse every core achievement from Super Mario Land (34 core).
#[test]
fn parse_sml_achievements() {
    let pd = load_fixture("sml_patch.json");
    let core: Vec<_> = pd.achievements.iter().filter(|a| a.flags == 3).collect();
    assert!(!core.is_empty(), "no core achievements in fixture");

    let mut failures = Vec::new();
    for a in &core {
        if let Err(e) = Achievement::parse(&a.memaddr) {
            failures.push(format!(
                "[{}] {}: {:?}\n  memaddr: {}",
                a.id, a.title, e, a.memaddr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} has {}/{} parse failures:\n{}",
        pd.title,
        failures.len(),
        core.len(),
        failures.join("\n")
    );
    eprintln!(
        "{}: parsed {}/{} core achievements OK",
        pd.title,
        core.len(),
        core.len()
    );
}

/// Parse every core achievement from Pokémon Red (91 core).
#[test]
fn parse_pokemon_red_achievements() {
    let pd = load_fixture("pokemon_red_patch.json");
    let core: Vec<_> = pd.achievements.iter().filter(|a| a.flags == 3).collect();
    assert!(!core.is_empty());

    let mut failures = Vec::new();
    for a in &core {
        if let Err(e) = Achievement::parse(&a.memaddr) {
            failures.push(format!(
                "[{}] {}: {:?}\n  memaddr: {}",
                a.id, a.title, e, a.memaddr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} has {}/{} parse failures:\n{}",
        pd.title,
        failures.len(),
        core.len(),
        failures.join("\n")
    );
    eprintln!(
        "{}: parsed {}/{} core achievements OK",
        pd.title,
        core.len(),
        core.len()
    );
}

/// Parse every core achievement from Kirby's Dream Land (35 core).
#[test]
fn parse_kirby_achievements() {
    let pd = load_fixture("kirby_patch.json");
    let core: Vec<_> = pd.achievements.iter().filter(|a| a.flags == 3).collect();
    assert!(!core.is_empty());

    let mut failures = Vec::new();
    for a in &core {
        if let Err(e) = Achievement::parse(&a.memaddr) {
            failures.push(format!(
                "[{}] {}: {:?}\n  memaddr: {}",
                a.id, a.title, e, a.memaddr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} has {}/{} parse failures:\n{}",
        pd.title,
        failures.len(),
        core.len(),
        failures.join("\n")
    );
    eprintln!(
        "{}: parsed {}/{} core achievements OK",
        pd.title,
        core.len(),
        core.len()
    );
}

/// Parse every leaderboard from all three fixtures.
#[test]
fn parse_all_leaderboards() {
    let fixtures = [
        "sml_patch.json",
        "pokemon_red_patch.json",
        "kirby_patch.json",
    ];
    let mut total = 0;
    let mut failures = Vec::new();

    for fixture in &fixtures {
        let pd = load_fixture(fixture);
        for lb in &pd.leaderboards {
            total += 1;
            if let Err(e) = Leaderboard::parse(&lb.mem) {
                failures.push(format!(
                    "[{}] {} ({}): {:?}\n  mem: {}",
                    lb.id, lb.title, pd.title, e, lb.mem
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{}/{} leaderboard parse failures:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
    eprintln!("parsed {total}/{total} leaderboards OK");
}

/// Parse ALL achievements across all fixtures (including unofficial).
/// Reports stats but only fails on core achievement parse errors.
#[test]
fn parse_all_achievements_stats() {
    let fixtures = [
        "sml_patch.json",
        "pokemon_red_patch.json",
        "kirby_patch.json",
    ];
    let mut total = 0;
    let mut core_ok = 0;
    let mut unofficial_ok = 0;
    let mut core_fail = 0;
    let mut unofficial_fail = 0;

    for fixture in &fixtures {
        let pd = load_fixture(fixture);
        for a in &pd.achievements {
            total += 1;
            let is_core = a.flags == 3;
            match Achievement::parse(&a.memaddr) {
                Ok(_) => {
                    if is_core {
                        core_ok += 1;
                    } else {
                        unofficial_ok += 1;
                    }
                }
                Err(_) => {
                    if is_core {
                        core_fail += 1;
                    } else {
                        unofficial_fail += 1;
                    }
                }
            }
        }
    }

    eprintln!("Total: {total} achievements");
    eprintln!("  Core: {core_ok} ok, {core_fail} failed");
    eprintln!("  Unofficial: {unofficial_ok} ok, {unofficial_fail} failed");
    assert_eq!(
        core_fail, 0,
        "{core_fail} core achievements failed to parse"
    );
}
