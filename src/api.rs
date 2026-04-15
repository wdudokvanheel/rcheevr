//! RA web API request/response types.
//!
//! Endpoints: `login2`, `patch`, `startsession`, `ping`, `awardachievement`,
//! `submitlbentry`, `gameid`.

pub const PRODUCTION_BASE_URL: &str = "https://retroachievements.org/dorequest.php";

#[cfg(test)]
pub fn test_base_url() -> &'static str {
    // Tests must point at a MockTransport or a local axum fixture. Never real RA.
    "http://127.0.0.1:0/mock-ra"
}
