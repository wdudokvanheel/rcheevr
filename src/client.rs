//! `AchievementRuntime` — top-level handle the app owns.
//!
//! Generic over `HttpTransport`. Holds session state, loaded game, evaluator,
//! and a pending event queue. The app drives it by supplying a per-frame
//! memory peek closure backed by the emulator core.

use crate::api;
use crate::api::RaResponse;
use crate::evaluator::{eval_achievement, AchievementState};
use crate::events::AchievementEvent;
use crate::hash::hash_gb;
use crate::http::{HttpTransport, TransportError};
use crate::leaderboard::{eval_leaderboard, LeaderboardEvent, LeaderboardState};
use crate::memaddr::MemSize;
use crate::patch::{LoadError, LoadedGame};
use serde::de::DeserializeOwned;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Active user session.
#[derive(Debug, Clone)]
pub struct Session {
    pub user: String,
    pub token: String,
    pub score: u32,
    pub softcore_score: u32,
}

/// Errors surfaced by [`AchievementRuntime`] methods.
#[derive(Debug)]
pub enum RuntimeError {
    Transport(TransportError),
    Api(String),
    Load(LoadError),
    NotLoggedIn,
    NoGame,
    InvalidResponse(String),
}

impl From<TransportError> for RuntimeError {
    fn from(e: TransportError) -> Self {
        RuntimeError::Transport(e)
    }
}

// ---------------------------------------------------------------------------
// RuntimeGame — internal per-game state
// ---------------------------------------------------------------------------

struct RuntimeGame {
    loaded: LoadedGame,
    achievement_states: Vec<AchievementState>,
    /// `true` once an achievement has been unlocked this session (no re-award).
    unlocked: Vec<bool>,
    leaderboard_states: Vec<LeaderboardState>,
    last_rich_presence: String,
    ping_counter: u32,
    rom_hash: String,
    pending_awards: Vec<PendingAward>,
    pending_lb_submits: Vec<PendingLeaderboardSubmit>,
}

#[derive(Debug, Clone)]
struct PendingAward {
    idx: usize,
    id: u32,
    title: String,
    points: u32,
    icon_url: String,
    rom_hash: String,
}

#[derive(Debug, Clone)]
struct PendingLeaderboardSubmit {
    id: u32,
    value: i64,
    rom_hash: String,
}

// ---------------------------------------------------------------------------
// AchievementRuntime
// ---------------------------------------------------------------------------

/// Top-level achievement-tracking state machine.
///
/// Generic over `H: HttpTransport` so tests can inject a [`MockTransport`] and
/// production builds can supply a blocking reqwest or wasm fetch wrapper.
///
/// [`MockTransport`]: crate::http::MockTransport
pub struct AchievementRuntime<H: HttpTransport> {
    http: H,
    base_url: String,
    session: Option<Session>,
    game: Option<RuntimeGame>,
    events: Vec<AchievementEvent>,
    hardcore: bool,
}

impl<H: HttpTransport> AchievementRuntime<H> {
    /// Create a runtime pointed at the real RA production endpoint.
    pub fn new(http: H) -> Self {
        Self::with_base_url(http, api::PRODUCTION_BASE_URL.to_string())
    }

    /// Create a runtime pointed at a custom base URL (useful for tests).
    pub fn with_base_url(http: H, base_url: String) -> Self {
        Self {
            http,
            base_url,
            session: None,
            game: None,
            events: Vec::new(),
            hardcore: false,
        }
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub fn hardcore(&self) -> bool {
        self.hardcore
    }

    /// Flip hardcore mode. If the new value differs from the current one, the
    /// session and loaded game are dropped so the app must re-login and
    /// re-load the ROM — matching RA's own behavior, since `startsession` must
    /// be re-issued under the new hardcore flag to register the mode switch
    /// server-side.
    pub fn set_hardcore(&mut self, on: bool) {
        if self.hardcore == on {
            return;
        }
        self.hardcore = on;
        self.session = None;
        self.game = None;
        self.events.clear();
    }

    pub fn loaded_game(&self) -> Option<&LoadedGame> {
        self.game.as_ref().map(|g| &g.loaded)
    }

    pub fn rich_presence(&self) -> &str {
        self.game
            .as_ref()
            .map(|g| g.last_rich_presence.as_str())
            .unwrap_or("")
    }

    // ------------------------------------------------------------------
    // Login
    // ------------------------------------------------------------------

    /// Authenticate with a plain-text password.
    pub async fn login_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<&Session, RuntimeError> {
        let url = self.base_url.clone();
        let hardcore_str = if self.hardcore { "1" } else { "0" };
        let form = [
            ("r", "login2"),
            ("u", user),
            ("p", password),
            ("h", hardcore_str),
        ];
        let bytes = self.http.post_form(&url, &form).await?;
        Self::handle_login_response(&bytes, &mut self.session)?;
        Ok(self.session.as_ref().unwrap())
    }

    /// Authenticate with a cached API token.
    pub async fn login_token(&mut self, user: &str, token: &str) -> Result<&Session, RuntimeError> {
        let url = self.base_url.clone();
        let hardcore_str = if self.hardcore { "1" } else { "0" };
        let form = [
            ("r", "login2"),
            ("u", user),
            ("t", token),
            ("h", hardcore_str),
        ];
        let bytes = self.http.post_form(&url, &form).await?;
        Self::handle_login_response(&bytes, &mut self.session)?;
        Ok(self.session.as_ref().unwrap())
    }

    fn handle_login_response(
        bytes: &[u8],
        session_slot: &mut Option<Session>,
    ) -> Result<(), RuntimeError> {
        let resp: RaResponse<api::login2::Login2Data> = serde_json::from_slice(bytes)
            .map_err(|e| RuntimeError::InvalidResponse(e.to_string()))?;

        if !resp.success {
            let msg = resp.error.unwrap_or_else(|| "login failed".to_string());
            return Err(RuntimeError::Api(msg));
        }

        let data = resp
            .data
            .ok_or_else(|| RuntimeError::InvalidResponse("missing login data".to_string()))?;

        *session_slot = Some(Session {
            user: data.display_name.unwrap_or_else(|| data.user.clone()),
            token: data.token.clone(),
            score: data.score,
            softcore_score: data.softcore_score,
        });

        Ok(())
    }

    fn parse_response<T: DeserializeOwned>(
        bytes: &[u8],
        endpoint: &'static str,
    ) -> Result<T, RuntimeError> {
        let resp: RaResponse<T> = serde_json::from_slice(bytes)
            .map_err(|e| RuntimeError::InvalidResponse(e.to_string()))?;

        if !resp.success {
            let msg = resp.error.unwrap_or_else(|| format!("{endpoint} failed"));
            return Err(RuntimeError::Api(msg));
        }

        resp.data
            .ok_or_else(|| RuntimeError::InvalidResponse(format!("missing {endpoint} data")))
    }

    fn parse_empty_response(bytes: &[u8], endpoint: &'static str) -> Result<(), RuntimeError> {
        let resp: RaResponse<serde_json::Value> = serde_json::from_slice(bytes)
            .map_err(|e| RuntimeError::InvalidResponse(e.to_string()))?;
        if resp.success {
            Ok(())
        } else {
            Err(RuntimeError::Api(
                resp.error.unwrap_or_else(|| format!("{endpoint} failed")),
            ))
        }
    }

    // ------------------------------------------------------------------
    // load_rom
    // ------------------------------------------------------------------

    /// Hash `rom`, look up its game ID, fetch the patch, and start a session.
    pub async fn load_rom(&mut self, rom: &[u8]) -> Result<&LoadedGame, RuntimeError> {
        let session = self.session.as_ref().ok_or(RuntimeError::NotLoggedIn)?;
        let user = session.user.clone();
        let token = session.token.clone();
        let rom_hash = hash_gb(rom);

        let url = self.base_url.clone();

        // Step 1: resolve game ID from ROM hash.
        let gameid_form = [("r", "gameid"), ("m", rom_hash.as_str())];
        let gameid_bytes = self.http.post_form(&url, &gameid_form).await?;
        let game_id =
            Self::parse_response::<api::gameid::GameIdData>(&gameid_bytes, "gameid")?.game_id;

        if game_id == 0 {
            return Err(RuntimeError::Api("unknown rom".to_string()));
        }

        let game_id_str = game_id.to_string();

        // Step 2: fetch patch.
        let patch_form = [
            ("r", "patch"),
            ("u", user.as_str()),
            ("t", token.as_str()),
            ("g", game_id_str.as_str()),
        ];
        let patch_bytes = self.http.post_form(&url, &patch_form).await?;

        let patch_json = std::str::from_utf8(&patch_bytes)
            .map_err(|e| RuntimeError::InvalidResponse(e.to_string()))?;

        let mut loaded = LoadedGame::from_patch_json(patch_json).map_err(RuntimeError::Load)?;
        loaded.game_id = game_id as u32;

        // Step 3: start session.
        let hardcore_val = if self.hardcore { "1" } else { "0" };
        let session_form = [
            ("r", "startsession"),
            ("u", user.as_str()),
            ("t", token.as_str()),
            ("g", game_id_str.as_str()),
            ("h", hardcore_val),
        ];
        let session_bytes = self.http.post_form(&url, &session_form).await?;

        // Parse already-unlocked IDs to avoid re-awarding them.
        let session_data = Self::parse_response::<api::startsession::StartSessionData>(
            &session_bytes,
            "startsession",
        )?;
        let unlocks = if self.hardcore {
            session_data.hardcore_unlocks
        } else {
            session_data.unlocks
        };
        let already_unlocked: std::collections::HashSet<u32> =
            unlocks.iter().map(|e| e.id as u32).collect();

        // Step 4: build per-achievement state.
        let achievement_states: Vec<AchievementState> = loaded
            .achievements
            .iter()
            .map(|a| AchievementState::for_achievement(&a.ast))
            .collect();

        let unlocked: Vec<bool> = loaded
            .achievements
            .iter()
            .map(|a| already_unlocked.contains(&a.id))
            .collect();

        let leaderboard_states: Vec<LeaderboardState> = loaded
            .leaderboards
            .iter()
            .map(|lb| LeaderboardState::for_leaderboard(&lb.ast))
            .collect();

        self.game = Some(RuntimeGame {
            loaded,
            achievement_states,
            unlocked,
            leaderboard_states,
            last_rich_presence: String::new(),
            ping_counter: 0,
            rom_hash,
            pending_awards: Vec::new(),
            pending_lb_submits: Vec::new(),
        });

        Ok(&self.game.as_ref().unwrap().loaded)
    }

    // ------------------------------------------------------------------
    // peek_from — per-frame driver
    // ------------------------------------------------------------------

    /// Drive one emulator frame.
    ///
    /// `peek` must be a side-effect-free closure that reads memory from the
    /// emulator core: `peek(address, size) -> value`.
    pub async fn peek_from<F: FnMut(u32, MemSize) -> u32>(&mut self, mut peek: F) {
        if self.game.is_none() {
            return;
        }

        let session = match &self.session {
            Some(s) => (s.user.clone(), s.token.clone()),
            None => return,
        };
        let (user, token) = session;
        let url = self.base_url.clone();
        let hardcore_val = if self.hardcore { "1" } else { "0" };

        // ------------------------------------------------------------------
        // Evaluate achievements.
        // Collect triggered info first, then do HTTP (avoids split-borrow issues).
        // ------------------------------------------------------------------
        let mut awards: Vec<PendingAward> = Vec::new();
        {
            let game = self.game.as_mut().unwrap();
            for idx in 0..game.loaded.achievements.len() {
                if game.unlocked[idx] {
                    continue;
                }
                let triggered = eval_achievement(
                    &game.loaded.achievements[idx].ast,
                    &mut game.achievement_states[idx],
                    &mut peek,
                );
                if triggered {
                    let ach = &game.loaded.achievements[idx];
                    awards.push(PendingAward {
                        idx,
                        id: ach.id,
                        title: ach.title.clone(),
                        points: ach.points,
                        icon_url: format!(
                            "https://media.retroachievements.org/Badge/{}.png",
                            ach.badge_name
                        ),
                        rom_hash: game.rom_hash.clone(),
                    });
                }
            }
            game.pending_awards.extend(awards);
        }

        let pending_awards = {
            let game = self.game.as_mut().unwrap();
            std::mem::take(&mut game.pending_awards)
        };
        let mut retry_awards = Vec::new();
        for info in pending_awards {
            let id_str = info.id.to_string();
            let award_form = [
                ("r", "awardachievement"),
                ("u", user.as_str()),
                ("t", token.as_str()),
                ("a", id_str.as_str()),
                ("h", hardcore_val),
                ("m", info.rom_hash.as_str()),
            ];
            let award_result = async {
                let bytes = self.http.post_form(&url, &award_form).await?;
                Self::parse_response::<api::awardachievement::AwardAchievementData>(
                    &bytes,
                    "awardachievement",
                )
            }
            .await;

            match award_result {
                Ok(_) => {
                    self.game.as_mut().unwrap().unlocked[info.idx] = true;
                    self.events.push(AchievementEvent::Unlocked {
                        id: info.id,
                        title: info.title,
                        points: info.points,
                        icon_url: info.icon_url,
                    });
                }
                Err(e) => {
                    self.events.push(AchievementEvent::ServerError(format!(
                        "awardachievement id={}: {:?}",
                        info.id, e
                    )));
                    retry_awards.push(info);
                }
            }
        }
        if !retry_awards.is_empty() {
            self.game
                .as_mut()
                .unwrap()
                .pending_awards
                .extend(retry_awards);
        }

        // ------------------------------------------------------------------
        // Evaluate leaderboards.
        // Collect events first, then do HTTP.
        // ------------------------------------------------------------------
        let mut lb_events: Vec<LeaderboardEvent> = Vec::new();
        let mut lb_submits: Vec<PendingLeaderboardSubmit> = Vec::new();
        {
            let game = self.game.as_mut().unwrap();
            let rom_hash = game.rom_hash.clone();
            for idx in 0..game.loaded.leaderboards.len() {
                let event = eval_leaderboard(
                    &game.loaded.leaderboards[idx].ast,
                    &mut game.leaderboard_states[idx],
                    &mut peek,
                );
                if let Some(ev) = event {
                    if let LeaderboardEvent::Submitted { id, value } = ev {
                        lb_submits.push(PendingLeaderboardSubmit {
                            id,
                            value,
                            rom_hash: rom_hash.clone(),
                        });
                    } else {
                        lb_events.push(ev);
                    }
                }
            }
            game.pending_lb_submits.extend(lb_submits);
        }

        for event in lb_events {
            match event {
                LeaderboardEvent::Started(id) => {
                    self.events.push(AchievementEvent::LeaderboardStarted(id));
                }
                LeaderboardEvent::Cancelled(id) => {
                    self.events.push(AchievementEvent::LeaderboardCancelled(id));
                }
                LeaderboardEvent::Submitted { .. } => unreachable!("submits are queued"),
            }
        }

        let pending_lb_submits = {
            let game = self.game.as_mut().unwrap();
            std::mem::take(&mut game.pending_lb_submits)
        };
        let mut retry_lb_submits = Vec::new();
        for info in pending_lb_submits {
            let id_str = info.id.to_string();
            let value_str = info.value.to_string();
            let submit_form = [
                ("r", "submitlbentry"),
                ("u", user.as_str()),
                ("t", token.as_str()),
                ("i", id_str.as_str()),
                ("s", value_str.as_str()),
                ("m", info.rom_hash.as_str()),
            ];
            let submit_result = async {
                let bytes = self.http.post_form(&url, &submit_form).await?;
                Self::parse_response::<api::submitlbentry::SubmitLbEntryData>(
                    &bytes,
                    "submitlbentry",
                )
            }
            .await;

            match submit_result {
                Ok(_) => {
                    self.events.push(AchievementEvent::LeaderboardSubmitted {
                        id: info.id,
                        score: info.value,
                    });
                }
                Err(e) => {
                    self.events.push(AchievementEvent::ServerError(format!(
                        "submitlbentry id={}: {:?}",
                        info.id, e
                    )));
                    retry_lb_submits.push(info);
                }
            }
        }
        if !retry_lb_submits.is_empty() {
            self.game
                .as_mut()
                .unwrap()
                .pending_lb_submits
                .extend(retry_lb_submits);
        }

        // ------------------------------------------------------------------
        // Update rich presence.
        // ------------------------------------------------------------------
        let game = self.game.as_mut().unwrap();
        let rp = game
            .loaded
            .rich_presence
            .render(|addr| peek(addr, MemSize::Byte));
        game.last_rich_presence = rp;

        // ------------------------------------------------------------------
        // Ping every ~120 frames.
        // ------------------------------------------------------------------
        game.ping_counter += 1;
        if game.ping_counter >= 120 {
            game.ping_counter = 0;
            let game_id_str = game.loaded.game_id.to_string();
            let rp_str = game.last_rich_presence.clone();
            let ping_form = [
                ("r", "ping"),
                ("u", user.as_str()),
                ("t", token.as_str()),
                ("g", game_id_str.as_str()),
                ("m", rp_str.as_str()),
            ];
            match self.http.post_form(&url, &ping_form).await {
                Ok(bytes) => {
                    if let Err(e) = Self::parse_empty_response(&bytes, "ping") {
                        self.events
                            .push(AchievementEvent::ServerError(format!("ping: {e:?}")));
                    }
                }
                Err(e) => self
                    .events
                    .push(AchievementEvent::ServerError(format!("ping: {e:?}"))),
            }
        }
    }

    // ------------------------------------------------------------------
    // drain_events
    // ------------------------------------------------------------------

    /// Drain and return all pending events.
    pub fn drain_events(&mut self) -> std::vec::Drain<'_, AchievementEvent> {
        self.events.drain(..)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockTransport;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Minimal patch JSON with one trivial achievement and one leaderboard.
    fn patch_json_one_ach() -> serde_json::Value {
        json!({
            "Success": true,
            "Title": "Test Game",
            "ImageIcon": "/Images/001.png",
            "ConsoleID": 4,
            "RichPresencePatch": "Display:\nPlaying",
            "Achievements": [
                {
                    "ID": 1,
                    "Title": "First",
                    "Description": "Get it",
                    "Points": 10,
                    "MemAddr": "0xH00c0=1",
                    "Flags": 3,
                    "Author": "dev",
                    "BadgeName": "00001"
                }
            ],
            "Leaderboards": []
        })
    }

    /// Patch JSON with one achievement AND one leaderboard.
    fn patch_json_ach_and_lb() -> serde_json::Value {
        json!({
            "Success": true,
            "Title": "Test Game",
            "ImageIcon": "/Images/001.png",
            "ConsoleID": 4,
            "RichPresencePatch": "Display:\nPlaying",
            "Achievements": [
                {
                    "ID": 1,
                    "Title": "First",
                    "Description": "Get it",
                    "Points": 10,
                    "MemAddr": "0xH00c0=1",
                    "Flags": 3,
                    "Author": "dev",
                    "BadgeName": "00001"
                }
            ],
            "Leaderboards": [
                {
                    "ID": 7,
                    "Title": "Speedrun",
                    "Description": "Fastest",
                    "Mem": "STA:0xH00d0=1::CAN:0xH00d0=0::SUB:0xH00d0=2::VAL:0xH00e0",
                    "Format": "SCORE"
                }
            ]
        })
    }

    fn gameid_resp(id: u64) -> serde_json::Value {
        json!({ "Success": true, "GameID": id })
    }

    fn login_ok_resp() -> serde_json::Value {
        json!({
            "Success": true,
            "User": "testuser",
            "DisplayName": "Test User",
            "Token": "mytoken",
            "Score": 500,
            "SoftcoreScore": 100,
            "Messages": 0,
            "Permissions": 0
        })
    }

    fn startsession_resp() -> serde_json::Value {
        json!({ "Success": true, "HardcoreUnlocks": [], "Unlocks": [] })
    }

    fn award_resp() -> serde_json::Value {
        json!({ "Success": true, "Score": 510, "SoftcoreScore": 100, "AchievementID": 1 })
    }

    fn award_error_resp() -> serde_json::Value {
        json!({ "Success": false, "Error": "temporary award failure" })
    }

    fn submit_lb_resp() -> serde_json::Value {
        json!({
            "Success": true,
            "Score": 42,
            "BestScore": 42,
            "RankInfo": { "Rank": 1, "NumEntries": 1 }
        })
    }

    /// Queue mock expectations for a full load_rom flow.
    fn queue_load_rom(t: &mut MockTransport, game_id: u64, patch: serde_json::Value) {
        t.expect_json("", &[("r", "gameid")], gameid_resp(game_id));
        t.expect_json("", &[("r", "patch")], patch);
        t.expect_json("", &[("r", "startsession")], startsession_resp());
    }

    // -----------------------------------------------------------------------
    // Test 1: new() with MockTransport compiles and creates a runtime.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn new_with_mock_transport() {
        let t = MockTransport::new();
        let rt = AchievementRuntime::new(t);
        assert!(rt.session().is_none());
        assert!(!rt.hardcore());
    }

    // -----------------------------------------------------------------------
    // Test 2: login_password success → session populated, returns Session.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn login_password_success() {
        let mut t = MockTransport::new();
        t.expect_json(
            "",
            &[("r", "login2"), ("u", "testuser"), ("p", "pass")],
            login_ok_resp(),
        );

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        let sess = rt.login_password("testuser", "pass").await.unwrap();

        assert_eq!(sess.user, "Test User");
        assert_eq!(sess.token, "mytoken");
        assert_eq!(sess.score, 500);
        assert_eq!(sess.softcore_score, 100);
        assert!(rt.session().is_some());
    }

    // -----------------------------------------------------------------------
    // Test 3: login_password failure → RuntimeError::Api.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn login_password_failure_returns_api_error() {
        let mut t = MockTransport::new();
        t.expect_json(
            "",
            &[("r", "login2")],
            json!({ "Success": false, "Error": "Invalid credentials" }),
        );

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        let err = rt.login_password("bad", "bad").await.unwrap_err();
        assert!(matches!(err, RuntimeError::Api(msg) if msg.contains("Invalid credentials")));
        assert!(rt.session().is_none());
    }

    // -----------------------------------------------------------------------
    // Test 4: login_token happy path.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn login_token_happy_path() {
        let mut t = MockTransport::new();
        t.expect_json(
            "",
            &[("r", "login2"), ("u", "testuser"), ("t", "mytoken")],
            login_ok_resp(),
        );

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        let sess = rt.login_token("testuser", "mytoken").await.unwrap();
        assert_eq!(sess.token, "mytoken");
    }

    // -----------------------------------------------------------------------
    // Test 5: load_rom happy path → game populated.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_rom_happy_path() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        let game = rt.load_rom(b"fake_rom_bytes").await.unwrap();

        assert_eq!(game.title, "Test Game");
        assert_eq!(game.game_id, 42);
        assert_eq!(game.achievements.len(), 1);
        assert_eq!(rt.rich_presence(), "");
    }

    // -----------------------------------------------------------------------
    // Test 6: load_rom unknown game (gameid returns 0) → RuntimeError::Api.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_rom_unknown_game() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        t.expect_json("", &[("r", "gameid")], gameid_resp(0));

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        let err = rt.load_rom(b"unknown_rom").await.unwrap_err();
        assert!(matches!(err, RuntimeError::Api(msg) if msg.contains("unknown rom")));
    }

    // -----------------------------------------------------------------------
    // Test 7: peek_from with trivial one-condition achievement triggers Unlocked
    //         event and records an awardachievement call.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn peek_from_triggers_unlocked_event_and_award_call() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        // Award response.
        t.expect_json("", &[("r", "awardachievement"), ("a", "1")], award_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Frame 1: all zeros — exits WAITING state (condition not met).
        rt.peek_from(|_, _| 0).await;
        assert!(rt.drain_events().next().is_none());

        // Frame 2: mem[0xc0] = 1 → achievement triggers.
        rt.peek_from(|addr, _size| if addr == 0xc0 { 1 } else { 0 })
            .await;

        let events: Vec<_> = rt.drain_events().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AchievementEvent::Unlocked { id: 1, .. })),
            "expected Unlocked event, got {:?} events",
            events.len()
        );

        // Verify transport recorded an awardachievement call.
        rt.http
            .assert_called("", &[("r", "awardachievement"), ("a", "1")]);
    }

    // -----------------------------------------------------------------------
    // Test 8: peek_from does not re-award already-unlocked achievements.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn peek_from_no_reawrd_on_second_frame() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        // Only one award expectation: a second call in strict mode would error.
        t.expect_json("", &[("r", "awardachievement")], award_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Frame 1: all zeros — exits WAITING state.
        rt.peek_from(|_, _| 0).await;
        let _ = rt.drain_events().count();

        // Frame 2: triggers.
        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;
        let _ = rt.drain_events().count();

        // Frame 3: same memory state — must NOT award again.
        rt.http.strict = false; // allow extra unmatched calls without panicking
        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;
        let events: Vec<_> = rt.drain_events().collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AchievementEvent::Unlocked { .. })),
            "should not re-emit Unlocked on second frame"
        );

        // awardachievement must have been called exactly once (check via recorded form fields).
        let award_calls = rt
            .http
            .calls
            .iter()
            .filter(|c| {
                c.form
                    .iter()
                    .any(|(k, v)| k == "r" && v == "awardachievement")
            })
            .count();
        assert_eq!(award_calls, 1, "expected exactly 1 awardachievement call");
    }

    #[tokio::test]
    async fn failed_award_is_retried_without_local_unlock() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        t.expect_json(
            "",
            &[("r", "awardachievement"), ("a", "1")],
            award_error_resp(),
        );
        t.expect_json("", &[("r", "awardachievement"), ("a", "1")], award_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        rt.peek_from(|_, _| 0).await;
        assert!(rt.drain_events().next().is_none());

        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;
        let first_attempt: Vec<_> = rt.drain_events().collect();
        assert!(
            first_attempt
                .iter()
                .any(|e| matches!(e, AchievementEvent::ServerError(_))),
            "expected a server error on failed award"
        );
        assert!(
            !first_attempt
                .iter()
                .any(|e| matches!(e, AchievementEvent::Unlocked { .. })),
            "local unlock must wait for server confirmation"
        );

        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;
        let retry_events: Vec<_> = rt.drain_events().collect();
        assert!(
            retry_events
                .iter()
                .any(|e| matches!(e, AchievementEvent::Unlocked { id: 1, .. })),
            "retry should emit unlock after server success"
        );

        let award_calls = rt
            .http
            .calls
            .iter()
            .filter(|c| {
                c.form
                    .iter()
                    .any(|(k, v)| k == "r" && v == "awardachievement")
            })
            .count();
        assert_eq!(award_calls, 2, "expected failed award plus retry");
    }

    // -----------------------------------------------------------------------
    // Test 9: leaderboard started then submitted across two frames.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn leaderboard_started_then_submitted() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_ach_and_lb());
        // Need an award expectation in case the achievement (mem[0xc0]=1) fires too.
        t.strict = false;

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Frame 1: mem[0xd0]=1 → STA fires. Achievement at 0xc0 not satisfied.
        rt.peek_from(|addr, _| match addr {
            0xd0 => 1,
            _ => 0,
        })
        .await;
        let events1: Vec<_> = rt.drain_events().collect();
        assert!(
            events1
                .iter()
                .any(|e| matches!(e, AchievementEvent::LeaderboardStarted(7))),
            "expected LeaderboardStarted(7)"
        );

        // Frame 2: mem[0xd0]=2 → SUB fires (submit), mem[0xe0]=42 (value).
        // Need to queue the submitlbentry response.
        rt.http.strict = false;
        rt.http
            .expect_json("", &[("r", "submitlbentry"), ("i", "7")], submit_lb_resp());

        rt.peek_from(|addr, _| match addr {
            0xd0 => 2,
            0xe0 => 42,
            _ => 0,
        })
        .await;
        let events2: Vec<_> = rt.drain_events().collect();
        assert!(
            events2.iter().any(|e| matches!(
                e,
                AchievementEvent::LeaderboardSubmitted { id: 7, score: 42 }
            )),
            "expected LeaderboardSubmitted(7, 42)"
        );

        rt.http
            .assert_called("", &[("r", "submitlbentry"), ("i", "7"), ("s", "42")]);
    }

    // -----------------------------------------------------------------------
    // Test 10: set_hardcore(true) causes h=1 on subsequent award calls.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_hardcore_sets_h1_on_award() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        t.expect_json("", &[("r", "awardachievement"), ("h", "1")], award_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.set_hardcore(true);
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Frame 1: all zeros — exits WAITING state.
        rt.peek_from(|_, _| 0).await;
        // Frame 2: triggers.
        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;

        rt.http
            .assert_called("", &[("r", "awardachievement"), ("h", "1")]);
    }

    // -----------------------------------------------------------------------
    // Test 10b: flipping hardcore mid-session drops session and game.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_hardcore_mid_session_drops_state() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();
        assert!(rt.session().is_some());
        assert!(rt.loaded_game().is_some());

        rt.set_hardcore(true);
        assert!(
            rt.session().is_none(),
            "session should be dropped on hardcore flip"
        );
        assert!(
            rt.loaded_game().is_none(),
            "loaded game should be dropped on hardcore flip"
        );
        assert!(rt.hardcore());
    }

    #[tokio::test]
    async fn set_hardcore_no_change_preserves_state() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Setting the same value is a no-op — don't drop state.
        rt.set_hardcore(false);
        assert!(rt.session().is_some());
        assert!(rt.loaded_game().is_some());
    }

    // -----------------------------------------------------------------------
    // Test 11: load_rom fails when not logged in.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_rom_requires_login() {
        let t = MockTransport::new();
        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        let err = rt.load_rom(b"rom").await.unwrap_err();
        assert!(matches!(err, RuntimeError::NotLoggedIn));
    }

    #[tokio::test]
    async fn load_rom_propagates_startsession_error() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        t.expect_json("", &[("r", "gameid")], gameid_resp(42));
        t.expect_json("", &[("r", "patch")], patch_json_one_ach());
        t.expect_json(
            "",
            &[("r", "startsession")],
            json!({ "Success": false, "Error": "session rejected" }),
        );

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        let err = rt.load_rom(b"rom").await.unwrap_err();
        assert!(matches!(err, RuntimeError::Api(msg) if msg == "session rejected"));
    }

    // -----------------------------------------------------------------------
    // Test 12: drain_events clears the queue.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn drain_events_clears_queue() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        t.expect_json("", &[("r", "awardachievement")], award_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        // Frame 1: all zeros — exits WAITING state.
        rt.peek_from(|_, _| 0).await;
        let _ = rt.drain_events().count();

        // Frame 2: triggers.
        rt.peek_from(|addr, _| if addr == 0xc0 { 1 } else { 0 })
            .await;
        let count1 = rt.drain_events().count();
        assert!(count1 > 0);

        // Third drain should be empty.
        let count2 = rt.drain_events().count();
        assert_eq!(count2, 0);
    }

    // -----------------------------------------------------------------------
    // Test 13: loaded_game() returns None before load_rom.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn loaded_game_none_before_load() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        assert!(rt.loaded_game().is_none());
    }

    // -----------------------------------------------------------------------
    // Test 14: rich_presence returns empty string before load_rom.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rich_presence_empty_before_load() {
        let t = MockTransport::new();
        let rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        assert_eq!(rt.rich_presence(), "");
    }

    // -----------------------------------------------------------------------
    // Test 15: rich_presence is updated after peek_from.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rich_presence_updated_after_peek() {
        let mut t = MockTransport::new();
        t.expect_json("", &[("r", "login2")], login_ok_resp());
        queue_load_rom(&mut t, 42, patch_json_one_ach());
        t.strict = false; // don't require awardachievement or ping expectations

        let mut rt = AchievementRuntime::with_base_url(t, "http://mock/".to_string());
        rt.login_password("testuser", "pass").await.unwrap();
        rt.load_rom(b"rom").await.unwrap();

        rt.peek_from(|_addr, _| 0).await;

        // "Display:\nPlaying" → should render "Playing".
        assert_eq!(rt.rich_presence(), "Playing");
    }
}
