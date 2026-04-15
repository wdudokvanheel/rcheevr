//! RA `patch` response parser.
//!
//! Converts the JSON patch blob (achievements, leaderboards, rich presence)
//! into internal AST types consumed by the evaluator.

use crate::api::patch::{AchievementDef, LeaderboardDef, PatchData};
use crate::api::RaResponse;
use crate::condition::{Achievement, ConditionParseError};
use crate::leaderboard::{Leaderboard, ValueFormat};
use crate::richpresence::{RichPresence, RichPresenceParseError};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Everything the evaluator needs after `load_rom` has fetched and parsed the
/// patch JSON. The raw JSON strings and `PatchData` are discarded; only the
/// fully-parsed ASTs are kept.
#[derive(Debug, Clone)]
pub struct LoadedGame {
    pub game_id: u32,
    pub title: String,
    pub console_id: u32,
    pub image_icon: String,
    pub achievements: Vec<LoadedAchievement>,
    pub leaderboards: Vec<LoadedLeaderboard>,
    pub rich_presence: RichPresence,
}

/// A single parsed achievement ready for the evaluator.
#[derive(Debug, Clone)]
pub struct LoadedAchievement {
    pub id: u32,
    pub title: String,
    pub description: String,
    pub points: u32,
    pub badge_name: String,
    pub author: String,
    /// RA flags field: 3 = Core, 5 = Unofficial. Both are kept; the runtime
    /// decides what to surface.
    pub flags: u32,
    pub ast: Achievement,
}

/// A single parsed leaderboard ready for the evaluator.
#[derive(Debug, Clone)]
pub struct LoadedLeaderboard {
    pub ast: Leaderboard,
}

/// Error returned when converting a patch payload into a `LoadedGame`.
#[derive(Debug, Clone)]
pub enum LoadError {
    /// A condition string inside an achievement failed to parse.
    Achievement { id: u32, error: ConditionParseError },
    /// A condition string inside a leaderboard failed to parse.
    Leaderboard { id: u32, error: ConditionParseError },
    /// The rich presence script failed to parse.
    RichPresence(RichPresenceParseError),
    /// JSON deserialisation failed.
    Json(String),
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_achievement(def: &AchievementDef) -> Result<LoadedAchievement, LoadError> {
    let ast = Achievement::parse(&def.memaddr).map_err(|e| LoadError::Achievement {
        id: def.id as u32,
        error: e,
    })?;
    Ok(LoadedAchievement {
        id: def.id as u32,
        title: def.title.clone(),
        description: def.description.clone(),
        points: def.points,
        badge_name: def.badge_name.clone(),
        author: def.author.clone(),
        flags: def.flags,
        ast,
    })
}

fn convert_leaderboard(def: &LeaderboardDef) -> Result<LoadedLeaderboard, LoadError> {
    let format = ValueFormat::parse(&def.format);
    let ast = Leaderboard::from_parts(
        def.id as u32,
        &def.title,
        &def.description,
        &def.mem,
        format,
    )
    .map_err(|e| LoadError::Leaderboard { id: def.id as u32, error: e })?;
    Ok(LoadedLeaderboard { ast })
}

// ---------------------------------------------------------------------------
// LoadedGame
// ---------------------------------------------------------------------------

impl LoadedGame {
    /// Convert an already-deserialized `PatchData` into a `LoadedGame`.
    ///
    /// `game_id` is not present on `PatchData` itself (the caller knows it from
    /// the request); we default to 0 when unavailable. Callers that know the ID
    /// should set it after construction if needed — but for our usage
    /// `from_patch_json` handles it by also accepting a bare `PatchData` without
    /// an outer game-id field.
    pub fn from_patch(data: &PatchData) -> Result<Self, LoadError> {
        // Parse achievements (preserve order, keep all flags).
        let achievements = data
            .achievements
            .iter()
            .map(convert_achievement)
            .collect::<Result<Vec<_>, _>>()?;

        // Parse leaderboards.
        let leaderboards = data
            .leaderboards
            .iter()
            .map(convert_leaderboard)
            .collect::<Result<Vec<_>, _>>()?;

        // Parse rich presence script.
        let rich_presence = RichPresence::parse(&data.rich_presence_patch)
            .map_err(LoadError::RichPresence)?;

        Ok(LoadedGame {
            // game_id is not carried inside PatchData; default to 0.
            // `from_patch_json` will override this if the wrapper supplies it.
            game_id: 0,
            title: data.title.clone(),
            console_id: data.console_id,
            image_icon: data.image_icon.clone(),
            achievements,
            leaderboards,
            rich_presence,
        })
    }

    /// Parse a raw RA `patch` endpoint JSON response into a `LoadedGame`.
    ///
    /// Tries two shapes:
    /// 1. `RaResponse<PatchData>` envelope (`{ "Success": true, "Title": ..., ... }`)
    /// 2. Bare `PatchData` object (no outer envelope)
    ///
    /// Returns `LoadError::Json` when both parses fail.
    pub fn from_patch_json(json: &str) -> Result<Self, LoadError> {
        // Attempt 1: unwrap from the RaResponse envelope.
        if let Ok(resp) = serde_json::from_str::<RaResponse<PatchData>>(json) {
            if let Some(data) = resp.data {
                return Self::from_patch(&data);
            }
            // Response parsed but data was None (unlikely for a valid patch response).
            return Err(LoadError::Json("missing data".into()));
        }

        // Attempt 2: bare PatchData without wrapper.
        match serde_json::from_str::<PatchData>(json) {
            Ok(data) => Self::from_patch(&data),
            Err(e) => Err(LoadError::Json(e.to_string())),
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
    // Test 1: Empty achievements and leaderboards → empty vecs.
    // -----------------------------------------------------------------------

    const EMPTY_WRAPPED: &str = r#"{
        "Success": true,
        "Title": "Empty Game",
        "ImageIcon": "/Images/000000.png",
        "ConsoleID": 1,
        "RichPresencePatch": "Display:\nPlaying",
        "Achievements": [],
        "Leaderboards": []
    }"#;

    #[test]
    fn empty_achievements_and_leaderboards() {
        let game = LoadedGame::from_patch_json(EMPTY_WRAPPED).unwrap();
        assert!(game.achievements.is_empty());
        assert!(game.leaderboards.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 2: One achievement with a trivial mem string → 1 condition in core.
    // -----------------------------------------------------------------------

    const ONE_ACHIEVEMENT: &str = r#"{
        "Success": true,
        "Title": "Test",
        "ImageIcon": "/Images/001.png",
        "ConsoleID": 4,
        "RichPresencePatch": "Display:\nHello",
        "Achievements": [
            {
                "ID": 1,
                "Title": "First",
                "Description": "Do the thing",
                "Points": 5,
                "MemAddr": "0xH00c0=1",
                "Flags": 3,
                "Author": "dev",
                "BadgeName": "00001"
            }
        ],
        "Leaderboards": []
    }"#;

    #[test]
    fn one_achievement_trivial_mem() {
        let game = LoadedGame::from_patch_json(ONE_ACHIEVEMENT).unwrap();
        assert_eq!(game.achievements.len(), 1);
        let ach = &game.achievements[0];
        assert_eq!(ach.id, 1);
        assert_eq!(ach.ast.core.conditions.len(), 1);
        assert!(ach.ast.alts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 3: One leaderboard with STA/CAN/SUB/VAL → ast.value is populated.
    // -----------------------------------------------------------------------

    const ONE_LEADERBOARD: &str = r#"{
        "Success": true,
        "Title": "Test",
        "ImageIcon": "/Images/001.png",
        "ConsoleID": 4,
        "RichPresencePatch": "Display:\nPlaying",
        "Achievements": [],
        "Leaderboards": [
            {
                "ID": 7,
                "Title": "High Score",
                "Description": "Best score",
                "Mem": "STA:0xH00c0=1::CAN:0xH00c0=0::SUB:0xH00c0=2::VAL:0xH00c1",
                "Format": "SCORE"
            }
        ]
    }"#;

    #[test]
    fn one_leaderboard_value_populated() {
        let game = LoadedGame::from_patch_json(ONE_LEADERBOARD).unwrap();
        assert_eq!(game.leaderboards.len(), 1);
        let lb = &game.leaderboards[0].ast;
        // VAL part: `0xH00c1` → one condition in value core condset.
        assert_eq!(lb.value.core.conditions.len(), 1);
        // STA / CAN / SUB also each have one condition.
        assert_eq!(lb.start.core.conditions.len(), 1);
        assert_eq!(lb.cancel.core.conditions.len(), 1);
        assert_eq!(lb.submit.core.conditions.len(), 1);
        assert_eq!(lb.format, ValueFormat::Score);
    }

    // -----------------------------------------------------------------------
    // Test 4: Rich presence `Display:\nHello` parses with one display line.
    // -----------------------------------------------------------------------

    const RP_HELLO: &str = r#"{
        "Success": true,
        "Title": "RP Test",
        "ImageIcon": "/Images/001.png",
        "ConsoleID": 1,
        "RichPresencePatch": "Display:\nHello",
        "Achievements": [],
        "Leaderboards": []
    }"#;

    #[test]
    fn rich_presence_hello_parses() {
        let game = LoadedGame::from_patch_json(RP_HELLO).unwrap();
        assert_eq!(game.rich_presence.display.len(), 1);
        // Render should produce "Hello".
        let rendered = game.rich_presence.render(|_| 0);
        assert_eq!(rendered, "Hello");
    }

    // -----------------------------------------------------------------------
    // Test 5: Achievement with invalid mem string → LoadError::Achievement.
    // -----------------------------------------------------------------------

    const BAD_ACH_MEM: &str = r#"{
        "Success": true,
        "Title": "Bad",
        "ImageIcon": "",
        "ConsoleID": 1,
        "RichPresencePatch": "Display:\nPlaying",
        "Achievements": [
            {
                "ID": 99,
                "Title": "Broken",
                "Description": "",
                "Points": 0,
                "MemAddr": "@invalid=5",
                "Flags": 3,
                "Author": "dev",
                "BadgeName": "00000"
            }
        ],
        "Leaderboards": []
    }"#;

    #[test]
    fn achievement_bad_mem_returns_error() {
        let err = LoadedGame::from_patch_json(BAD_ACH_MEM).unwrap_err();
        match err {
            LoadError::Achievement { id, .. } => assert_eq!(id, 99),
            other => panic!("expected LoadError::Achievement, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 6: Leaderboard with malformed mem → LoadError::Leaderboard.
    // -----------------------------------------------------------------------

    const BAD_LB_MEM: &str = r#"{
        "Success": true,
        "Title": "Bad LB",
        "ImageIcon": "",
        "ConsoleID": 1,
        "RichPresencePatch": "Display:\nPlaying",
        "Achievements": [],
        "Leaderboards": [
            {
                "ID": 42,
                "Title": "Broken LB",
                "Description": "",
                "Mem": "STA:@bad=1::CAN:0xH0001=0::SUB:0xH0001=2::VAL:0xH0002",
                "Format": "VALUE"
            }
        ]
    }"#;

    #[test]
    fn leaderboard_bad_mem_returns_error() {
        let err = LoadedGame::from_patch_json(BAD_LB_MEM).unwrap_err();
        match err {
            LoadError::Leaderboard { id, .. } => assert_eq!(id, 42),
            other => panic!("expected LoadError::Leaderboard, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 7: Full PatchData blob wrapped in `{ "Success": true, ... }`.
    // -----------------------------------------------------------------------

    const FULL_WRAPPED: &str = r#"{
        "Success": true,
        "Title": "Tetris",
        "ImageIcon": "/Images/001234.png",
        "ConsoleID": 4,
        "RichPresencePatch": "Display:\nPlaying Tetris",
        "Achievements": [
            {
                "ID": 42,
                "Title": "First Clear",
                "Description": "Clear your first line",
                "Points": 10,
                "MemAddr": "0xH00c0=1",
                "Flags": 3,
                "Author": "somedev",
                "BadgeName": "00001"
            }
        ],
        "Leaderboards": [
            {
                "ID": 7,
                "Title": "High Score",
                "Description": "Highest score achieved",
                "Mem": "STA:0xH00c0=1::CAN:0xH00c0=0::SUB:0xH00c0=2::VAL:0xH00c1",
                "Format": "SCORE"
            }
        ]
    }"#;

    #[test]
    fn full_blob_wrapped_in_ra_response() {
        let game = LoadedGame::from_patch_json(FULL_WRAPPED).unwrap();
        assert_eq!(game.title, "Tetris");
        assert_eq!(game.achievements.len(), 1);
        assert_eq!(game.leaderboards.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 8: Bare PatchData without the RaResponse wrapper.
    // -----------------------------------------------------------------------

    const BARE_PATCH_DATA: &str = r#"{
        "Title": "Bare Game",
        "ImageIcon": "/Images/bare.png",
        "ConsoleID": 6,
        "RichPresencePatch": "Display:\nBare",
        "Achievements": [],
        "Leaderboards": []
    }"#;

    #[test]
    fn bare_patch_data_no_wrapper() {
        let game = LoadedGame::from_patch_json(BARE_PATCH_DATA).unwrap();
        assert_eq!(game.title, "Bare Game");
        assert_eq!(game.console_id, 6);
    }

    // -----------------------------------------------------------------------
    // Test 9: Multiple achievements preserved in order.
    // -----------------------------------------------------------------------

    const MULTI_ACH: &str = r#"{
        "Success": true,
        "Title": "Multi",
        "ImageIcon": "",
        "ConsoleID": 1,
        "RichPresencePatch": "Display:\nPlaying",
        "Achievements": [
            {
                "ID": 10, "Title": "A", "Description": "", "Points": 1,
                "MemAddr": "0xH0000=1", "Flags": 3, "Author": "dev", "BadgeName": "1"
            },
            {
                "ID": 20, "Title": "B", "Description": "", "Points": 2,
                "MemAddr": "0xH0001=2", "Flags": 5, "Author": "dev", "BadgeName": "2"
            },
            {
                "ID": 30, "Title": "C", "Description": "", "Points": 3,
                "MemAddr": "0xH0002=3", "Flags": 3, "Author": "dev", "BadgeName": "3"
            }
        ],
        "Leaderboards": []
    }"#;

    #[test]
    fn multiple_achievements_preserved_in_order() {
        let game = LoadedGame::from_patch_json(MULTI_ACH).unwrap();
        assert_eq!(game.achievements.len(), 3);
        assert_eq!(game.achievements[0].id, 10);
        assert_eq!(game.achievements[1].id, 20);
        assert_eq!(game.achievements[2].id, 30);
        // Both Core (3) and Unofficial (5) flags are kept.
        assert_eq!(game.achievements[0].flags, 3);
        assert_eq!(game.achievements[1].flags, 5);
    }

    // -----------------------------------------------------------------------
    // Test 10: Game metadata propagated correctly.
    // -----------------------------------------------------------------------

    const METADATA: &str = r#"{
        "Success": true,
        "Title": "Super Mario Land",
        "ImageIcon": "/Images/000123.png",
        "ConsoleID": 4,
        "RichPresencePatch": "Display:\nPlaying Super Mario Land",
        "Achievements": [],
        "Leaderboards": []
    }"#;

    #[test]
    fn game_metadata_propagated() {
        let game = LoadedGame::from_patch_json(METADATA).unwrap();
        assert_eq!(game.title, "Super Mario Land");
        assert_eq!(game.image_icon, "/Images/000123.png");
        assert_eq!(game.console_id, 4);
        // game_id defaults to 0 when not available in PatchData.
        assert_eq!(game.game_id, 0);
    }
}
