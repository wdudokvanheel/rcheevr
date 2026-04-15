//! RA web API request/response types.
//!
//! Endpoints: `login2`, `patch`, `startsession`, `ping`, `awardachievement`,
//! `submitlbentry`, `gameid`.

use serde::{Deserialize, Serialize};

pub const PRODUCTION_BASE_URL: &str = "https://retroachievements.org/dorequest.php";

#[cfg(test)]
pub fn test_base_url() -> &'static str {
    // Tests must point at a MockTransport or a local axum fixture. Never real RA.
    "http://127.0.0.1:0/mock-ra"
}

// ---------------------------------------------------------------------------
// Generic response envelope
// ---------------------------------------------------------------------------

/// The common RA response envelope. Every endpoint wraps its payload here.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RaResponse<T> {
    #[serde(rename = "Success")]
    pub success: bool,
    #[serde(rename = "Error", default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Option<T>,
}

// ---------------------------------------------------------------------------
// login2
// ---------------------------------------------------------------------------

pub mod login2 {
    use serde::{Deserialize, Serialize};

    /// Form fields for `login2`. Send either `password` or `token`, not both.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Login2Request {
        #[serde(rename = "u")]
        pub username: String,
        /// Plain-text password (used when the caller has no cached token).
        #[serde(rename = "p", skip_serializing_if = "Option::is_none")]
        pub password: Option<String>,
        /// Previously issued API token (preferred over password after first login).
        #[serde(rename = "t", skip_serializing_if = "Option::is_none")]
        pub token: Option<String>,
    }

    /// Fields returned inside the `RaResponse` envelope for `login2`.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Login2Data {
        #[serde(rename = "User")]
        pub user: String,
        #[serde(rename = "DisplayName")]
        pub display_name: String,
        /// Reusable API token for subsequent requests (store in keyring).
        #[serde(rename = "Token")]
        pub token: String,
        /// Hardcore + softcore total.
        #[serde(rename = "Score")]
        pub score: u32,
        /// Softcore-only score.
        #[serde(rename = "SoftcoreScore")]
        pub softcore_score: u32,
        /// Unread message count.
        #[serde(rename = "Messages")]
        pub messages: u32,
        /// Permission level (0 = regular user, higher = staff).
        #[serde(rename = "Permissions")]
        pub permissions: u32,
    }
}

// ---------------------------------------------------------------------------
// gameid
// ---------------------------------------------------------------------------

pub mod gameid {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `gameid` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct GameIdRequest {
        /// MD5 hash of the ROM (32-char lowercase hex from `hash_gb` / `hash_gba`).
        #[serde(rename = "m")]
        pub md5: String,
    }

    /// Response payload for `gameid`. `game_id` is 0 when the ROM is unknown.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct GameIdData {
        #[serde(rename = "GameID")]
        pub game_id: u64,
    }
}

// ---------------------------------------------------------------------------
// patch
// ---------------------------------------------------------------------------

pub mod patch {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `patch` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct PatchRequest {
        #[serde(rename = "u")]
        pub user: String,
        #[serde(rename = "t")]
        pub token: String,
        #[serde(rename = "g")]
        pub game_id: u64,
    }

    /// The full game patch data returned by the `patch` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct PatchData {
        #[serde(rename = "Title")]
        pub title: String,
        #[serde(rename = "ImageIcon")]
        pub image_icon: String,
        #[serde(rename = "ConsoleID")]
        pub console_id: u32,
        /// The game's rich-presence script (may be empty string).
        #[serde(rename = "RichPresencePatch")]
        pub rich_presence_patch: String,
        #[serde(rename = "Achievements")]
        pub achievements: Vec<AchievementDef>,
        #[serde(rename = "Leaderboards")]
        pub leaderboards: Vec<LeaderboardDef>,
    }

    /// A single achievement definition from the patch payload.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct AchievementDef {
        #[serde(rename = "ID")]
        pub id: u64,
        #[serde(rename = "Title")]
        pub title: String,
        #[serde(rename = "Description")]
        pub description: String,
        #[serde(rename = "Points")]
        pub points: u32,
        /// Condition expression string (rcheevos memaddr format).
        #[serde(rename = "MemAddr")]
        pub memaddr: String,
        /// Bitmask: 3 = core, 5 = unofficial, etc.
        #[serde(rename = "Flags")]
        pub flags: u32,
        #[serde(rename = "Author")]
        pub author: String,
        #[serde(rename = "BadgeName")]
        pub badge_name: String,
    }

    /// A single leaderboard definition from the patch payload.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct LeaderboardDef {
        #[serde(rename = "ID")]
        pub id: u64,
        #[serde(rename = "Title")]
        pub title: String,
        #[serde(rename = "Description")]
        pub description: String,
        /// Combined START/CANCEL/SUBMIT/VALUE condition string.
        #[serde(rename = "Mem")]
        pub mem: String,
        /// Display format: SCORE, TIME, FRAMES, etc.
        #[serde(rename = "Format")]
        pub format: String,
    }
}

// ---------------------------------------------------------------------------
// startsession
// ---------------------------------------------------------------------------

pub mod startsession {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `startsession` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct StartSessionRequest {
        #[serde(rename = "u")]
        pub user: String,
        #[serde(rename = "t")]
        pub token: String,
        #[serde(rename = "g")]
        pub game_id: u64,
        /// 1 = hardcore, 0 = softcore.
        #[serde(rename = "h")]
        pub hardcore: u8,
    }

    /// An unlocked achievement id inside the session response.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct UnlockEntry {
        #[serde(rename = "ID")]
        pub id: u64,
    }

    /// Response payload for `startsession`.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct StartSessionData {
        /// IDs already unlocked in hardcore mode for this game.
        #[serde(rename = "HardcoreUnlocks", default)]
        pub hardcore_unlocks: Vec<UnlockEntry>,
        /// IDs already unlocked in softcore mode for this game.
        #[serde(rename = "Unlocks", default)]
        pub unlocks: Vec<UnlockEntry>,
    }
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

pub mod ping {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `ping` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct PingRequest {
        #[serde(rename = "u")]
        pub user: String,
        #[serde(rename = "t")]
        pub token: String,
        #[serde(rename = "g")]
        pub game_id: u64,
        /// Current rich presence string (empty string is fine).
        #[serde(rename = "m")]
        pub rich_presence: String,
    }

    /// `ping` returns only the standard envelope `Success` field; no extra data.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct PingData {}
}

// ---------------------------------------------------------------------------
// awardachievement
// ---------------------------------------------------------------------------

pub mod awardachievement {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `awardachievement` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct AwardAchievementRequest {
        #[serde(rename = "u")]
        pub user: String,
        #[serde(rename = "t")]
        pub token: String,
        /// Achievement ID to award.
        #[serde(rename = "a")]
        pub achievement_id: u64,
        /// 1 = hardcore, 0 = softcore.
        #[serde(rename = "h")]
        pub hardcore: u8,
        /// ROM MD5 hash (ties the award to a specific ROM version).
        #[serde(rename = "m")]
        pub hash: String,
    }

    /// Response payload for `awardachievement`.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct AwardAchievementData {
        /// New total score (hardcore + softcore) after the award.
        #[serde(rename = "Score")]
        pub score: u32,
        /// New softcore score after the award.
        #[serde(rename = "SoftcoreScore")]
        pub softcore_score: u32,
        /// ID of the achievement that was awarded (echoed back for verification).
        #[serde(rename = "AchievementID")]
        pub achievement_id: u64,
    }
}

// ---------------------------------------------------------------------------
// submitlbentry
// ---------------------------------------------------------------------------

pub mod submitlbentry {
    use serde::{Deserialize, Serialize};

    /// Form fields for the `submitlbentry` endpoint.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct SubmitLbEntryRequest {
        #[serde(rename = "u")]
        pub user: String,
        #[serde(rename = "t")]
        pub token: String,
        /// Leaderboard ID.
        #[serde(rename = "i")]
        pub lb_id: u64,
        /// Raw integer score value.
        #[serde(rename = "s")]
        pub score: i64,
        /// ROM MD5 hash.
        #[serde(rename = "m")]
        pub hash: String,
    }

    /// Ranking information nested inside `SubmitLbEntryData`.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct LbRankInfo {
        /// Player's new rank on the leaderboard.
        #[serde(rename = "Rank")]
        pub rank: u32,
        /// Total number of entries on the board.
        #[serde(rename = "NumEntries")]
        pub num_entries: u32,
    }

    /// Response payload for `submitlbentry`.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct SubmitLbEntryData {
        /// The score just submitted (echoed back).
        #[serde(rename = "Score")]
        pub score: i64,
        /// The player's all-time best score on this board.
        #[serde(rename = "BestScore")]
        pub best_score: i64,
        #[serde(rename = "RankInfo")]
        pub rank_info: LbRankInfo,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- login2 --

    #[test]
    fn login2_response_roundtrip() {
        let json = r#"{
            "Success": true,
            "User": "testuser",
            "DisplayName": "Test User",
            "Token": "abc123token",
            "Score": 500,
            "SoftcoreScore": 100,
            "Messages": 2,
            "Permissions": 0
        }"#;

        let resp: RaResponse<login2::Login2Data> = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert!(resp.error.is_none());
        let data = resp.data.unwrap();
        assert_eq!(data.display_name, "Test User");
        assert_eq!(data.token, "abc123token");
        assert_eq!(data.score, 500);
        assert_eq!(data.softcore_score, 100);
        assert_eq!(data.messages, 2);
        assert_eq!(data.permissions, 0);
    }

    #[test]
    fn login2_error_response_roundtrip() {
        let json = r#"{"Success": false, "Error": "Invalid credentials"}"#;
        let resp: RaResponse<login2::Login2Data> = serde_json::from_str(json).unwrap();
        assert!(!resp.success);
        assert_eq!(resp.error.as_deref(), Some("Invalid credentials"));
        assert!(resp.data.is_none());
    }

    // -- gameid --

    #[test]
    fn gameid_response_roundtrip() {
        let json = r#"{"Success": true, "GameID": 1234}"#;
        let resp: RaResponse<gameid::GameIdData> = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.data.unwrap().game_id, 1234);
    }

    #[test]
    fn gameid_unknown_rom_returns_zero() {
        let json = r#"{"Success": true, "GameID": 0}"#;
        let resp: RaResponse<gameid::GameIdData> = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.unwrap().game_id, 0);
    }

    // -- patch --

    #[test]
    fn patch_response_roundtrip() {
        let json = r#"{
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

        let resp: RaResponse<patch::PatchData> = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        let data = resp.data.unwrap();
        assert_eq!(data.title, "Tetris");
        assert_eq!(data.console_id, 4);
        assert_eq!(data.achievements.len(), 1);
        assert_eq!(data.achievements[0].id, 42);
        assert_eq!(data.achievements[0].flags, 3);
        assert_eq!(data.leaderboards.len(), 1);
        assert_eq!(data.leaderboards[0].format, "SCORE");
    }

    // -- startsession --

    #[test]
    fn startsession_response_roundtrip() {
        let json = r#"{
            "Success": true,
            "HardcoreUnlocks": [{"ID": 1}, {"ID": 2}],
            "Unlocks": [{"ID": 1}, {"ID": 2}, {"ID": 3}]
        }"#;

        let resp: RaResponse<startsession::StartSessionData> =
            serde_json::from_str(json).unwrap();
        assert!(resp.success);
        let data = resp.data.unwrap();
        assert_eq!(data.hardcore_unlocks.len(), 2);
        assert_eq!(data.unlocks.len(), 3);
        assert_eq!(data.unlocks[2].id, 3);
    }

    // -- awardachievement --

    #[test]
    fn awardachievement_response_roundtrip() {
        let json = r#"{
            "Success": true,
            "Score": 510,
            "SoftcoreScore": 100,
            "AchievementID": 42
        }"#;

        let resp: RaResponse<awardachievement::AwardAchievementData> =
            serde_json::from_str(json).unwrap();
        assert!(resp.success);
        let data = resp.data.unwrap();
        assert_eq!(data.score, 510);
        assert_eq!(data.achievement_id, 42);
    }

    // -- submitlbentry --

    #[test]
    fn submitlbentry_response_roundtrip() {
        let json = r#"{
            "Success": true,
            "Score": 99000,
            "BestScore": 120000,
            "RankInfo": {"Rank": 5, "NumEntries": 200}
        }"#;

        let resp: RaResponse<submitlbentry::SubmitLbEntryData> =
            serde_json::from_str(json).unwrap();
        assert!(resp.success);
        let data = resp.data.unwrap();
        assert_eq!(data.best_score, 120000);
        assert_eq!(data.rank_info.rank, 5);
        assert_eq!(data.rank_info.num_entries, 200);
    }

    // -- ping --

    #[test]
    fn ping_response_roundtrip() {
        let json = r#"{"Success": true}"#;
        let resp: RaResponse<ping::PingData> = serde_json::from_str(json).unwrap();
        assert!(resp.success);
    }

    // -- request serialisation sanity checks --

    #[test]
    fn login2_request_password_serializes() {
        let req = login2::Login2Request {
            username: "user".into(),
            password: Some("pass".into()),
            token: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""u":"user""#));
        assert!(s.contains(r#""p":"pass""#));
        assert!(!s.contains(r#""t""#)); // token must be absent
    }

    #[test]
    fn login2_request_token_serializes() {
        let req = login2::Login2Request {
            username: "user".into(),
            password: None,
            token: Some("tok".into()),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""t":"tok""#));
        assert!(!s.contains(r#""p""#)); // password must be absent
    }
}
