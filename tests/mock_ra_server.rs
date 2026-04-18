//! Integration tests: `AchievementRuntime` over real HTTP against an axum-based
//! mock RA server.
//!
//! The whole file is gated behind `http-reqwest` so it only compiles/runs when
//! that feature is enabled.
#![cfg(feature = "http-reqwest")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::routing::post;
use axum::{Json, Router};

use rveevr::client::AchievementRuntime;
use rveevr::http::ReqwestTransport;

// ---------------------------------------------------------------------------
// Mock server state
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct MockServerState {
    calls: Vec<(String, HashMap<String, String>)>,
    awarded_ids: Vec<u32>,
    submitted_lbs: Vec<(u32, i64)>,
}

// ---------------------------------------------------------------------------
// MockRaServer
// ---------------------------------------------------------------------------

struct MockRaServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    runtime: Option<tokio::runtime::Runtime>,
    state: Arc<Mutex<MockServerState>>,
}

impl MockRaServer {
    fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let state = Arc::new(Mutex::new(MockServerState::default()));
        let state_clone = state.clone();

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        runtime.spawn(async move {
            let app = Router::new()
                .route("/dorequest.php", post(handler))
                .with_state(state_clone);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            addr_tx.send(addr).unwrap();

            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let addr = addr_rx.recv().unwrap();

        Self {
            addr,
            shutdown: Some(shutdown_tx),
            runtime: Some(runtime),
            state,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/dorequest.php", self.addr)
    }

    fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        drop(self.runtime.take());
    }

    fn awarded_ids(&self) -> Vec<u32> {
        self.state.lock().unwrap().awarded_ids.clone()
    }

    fn calls_for(&self, request_type: &str) -> Vec<HashMap<String, String>> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(r, _)| r == request_type)
            .map(|(_, form)| form.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Axum handler
// ---------------------------------------------------------------------------

async fn handler(
    State(state): State<Arc<Mutex<MockServerState>>>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let request_type = form.get("r").cloned().unwrap_or_default();

    // Record the call before checking the password so all calls are visible.
    state
        .lock()
        .unwrap()
        .calls
        .push((request_type.clone(), form.clone()));

    match request_type.as_str() {
        "login2" => {
            // Support simulating a failed login when password is "wrong".
            if form.get("p").map(String::as_str) == Some("wrong") {
                return Json(serde_json::json!({
                    "Success": false,
                    "Error": "Invalid credentials"
                }));
            }
            Json(serde_json::json!({
                "Success": true,
                "User": "tester",
                "Token": "TOKEN-abc",
                "DisplayName": "Tester",
                "Score": 0,
                "SoftcoreScore": 0,
                "Messages": 0,
                "Permissions": 0
            }))
        }

        "gameid" => {
            // Return GameID 0 for the specific MD5 of an all-zeros 16-byte ROM
            // (so `unknown_game_returns_error` can use a different ROM).
            //
            // MD5 of 16 zero bytes: "4ae71336e44bf9bf79d2752e234818a5"
            let md5 = form.get("m").map(String::as_str).unwrap_or("");
            if md5 == "4ae71336e44bf9bf79d2752e234818a5" {
                Json(serde_json::json!({ "Success": true, "GameID": 0 }))
            } else {
                Json(serde_json::json!({ "Success": true, "GameID": 1 }))
            }
        }

        // patch — PatchData fields are flattened into the RaResponse envelope.
        "patch" => Json(serde_json::json!({
            "Success": true,
            "Title": "Test Game",
            "ImageIcon": "",
            "ConsoleID": 4,
            "RichPresencePatch": "Display:\nHello",
            "Achievements": [{
                "ID": 10,
                "Title": "First byte is 1",
                "Description": "",
                "Points": 5,
                "MemAddr": "0xH0000=1",
                "Flags": 3,
                "Author": "",
                "BadgeName": "",
            }],
            "Leaderboards": []
        })),

        "startsession" => Json(serde_json::json!({
            "Success": true,
            "HardcoreUnlocks": [],
            "Unlocks": [],
            "ServerNow": 0
        })),

        "awardachievement" => {
            let id: u32 = form.get("a").and_then(|s| s.parse().ok()).unwrap_or(0);
            state.lock().unwrap().awarded_ids.push(id);
            Json(serde_json::json!({
                "Success": true,
                "Score": 5,
                "SoftcoreScore": 0,
                "AchievementID": id
            }))
        }

        "submitlbentry" => {
            let id: u32 = form.get("i").and_then(|s| s.parse().ok()).unwrap_or(0);
            let score: i64 = form.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
            state.lock().unwrap().submitted_lbs.push((id, score));
            Json(serde_json::json!({
                "Success": true,
                "Score": score,
                "BestScore": score,
                "RankInfo": { "Rank": 1, "NumEntries": 1 }
            }))
        }

        "ping" => Json(serde_json::json!({ "Success": true })),

        _ => Json(serde_json::json!({ "Success": false, "Error": "unknown request" })),
    }
}

// ---------------------------------------------------------------------------
// Helper: build a runtime pointed at `url`.
// ---------------------------------------------------------------------------

fn make_runtime(url: String) -> AchievementRuntime<ReqwestTransport> {
    AchievementRuntime::with_base_url(ReqwestTransport::new().unwrap(), url)
}

// ---------------------------------------------------------------------------
// Test 1: login → load_rom → peek_from → Unlocked event + awardachievement call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_then_load_rom_then_peek_flow() {
    let server = MockRaServer::start();
    let url = server.url();

    let mut rt = make_runtime(url);

    // Login.
    rt.login_password("tester", "pw")
        .await
        .expect("login failed");

    // Load a non-zero ROM (so gameid returns 1, not 0).
    let rom: Vec<u8> = {
        let mut v = vec![0u8; 16];
        v[0] = 0xff; // MD5 will differ from the all-zeros sentinel
        v
    };
    rt.load_rom(&rom).await.expect("load_rom failed");

    // Frame 1: address 0 = 0 → exits WAITING.
    rt.peek_from(|_addr, _size| 0).await;
    assert!(rt.drain_events().next().is_none());

    // Frame 2: address 0 = 1 → achievement 10 triggers (MemAddr: "0xH0000=1").
    rt.peek_from(|addr, _size| if addr == 0 { 1 } else { 0 })
        .await;

    // Exactly one Unlocked event for id 10.
    let events: Vec<_> = rt.drain_events().collect();
    let unlocked_ids: Vec<u32> = events
        .iter()
        .filter_map(|e| {
            if let rveevr::events::AchievementEvent::Unlocked { id, .. } = e {
                Some(*id)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        unlocked_ids,
        vec![10],
        "expected exactly one Unlocked event for id=10, got {} total events",
        events.len()
    );

    // Server must have recorded an awardachievement with a=10.
    let awarded = server.awarded_ids();
    assert_eq!(awarded, vec![10], "server did not record award for id=10");

    // Frame 3: same memory — must NOT re-award.
    rt.peek_from(|addr, _size| if addr == 0 { 1 } else { 0 })
        .await;
    let events2: Vec<_> = rt.drain_events().collect();
    assert!(
        !events2
            .iter()
            .any(|e| matches!(e, rveevr::events::AchievementEvent::Unlocked { .. })),
        "should not re-unlock on frame 2"
    );

    server.stop();
}

// ---------------------------------------------------------------------------
// Test 2: login with wrong password → RuntimeError::Api
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_failure_propagates() {
    let server = MockRaServer::start();
    let url = server.url();

    let mut rt = make_runtime(url);

    let err = rt
        .login_password("tester", "wrong")
        .await
        .expect_err("expected login failure");

    assert!(
        matches!(err, rveevr::client::RuntimeError::Api(_)),
        "expected RuntimeError::Api, got {:?}",
        err
    );

    server.stop();
}

// ---------------------------------------------------------------------------
// Test 3: unknown game (GameID 0) → RuntimeError::Api
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_game_returns_error() {
    let server = MockRaServer::start();
    let url = server.url();

    let mut rt = make_runtime(url);

    rt.login_password("tester", "pw")
        .await
        .expect("login failed");

    // All-zeros 16-byte ROM → MD5 "4ae71336e44bf9bf79d2752e234818a5" → GameID 0.
    let rom = vec![0u8; 16];
    let err = rt
        .load_rom(&rom)
        .await
        .expect_err("expected load_rom to fail for unknown game");

    assert!(
        matches!(err, rveevr::client::RuntimeError::Api(_)),
        "expected RuntimeError::Api for unknown game, got {:?}",
        err
    );

    server.stop();
}

// ---------------------------------------------------------------------------
// Test 4: ping is sent after enough frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ping_sent_after_enough_frames() {
    let server = MockRaServer::start();
    let url = server.url();

    let mut rt = make_runtime(url);

    rt.login_password("tester", "pw")
        .await
        .expect("login failed");

    let rom: Vec<u8> = {
        let mut v = vec![0u8; 16];
        v[0] = 0xab;
        v
    };
    rt.load_rom(&rom).await.expect("load_rom failed");

    // Drive 121 frames — ping fires after 120.
    for _ in 0..121 {
        rt.peek_from(|_addr, _size| 0).await;
        let _ = rt.drain_events().count();
    }

    let ping_calls = server.calls_for("ping");
    assert!(
        !ping_calls.is_empty(),
        "expected at least one ping call after 121 frames, got 0"
    );

    server.stop();
}
