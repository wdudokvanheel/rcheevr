//! `AchievementRuntime` — top-level handle the app owns.
//!
//! Generic over `HttpTransport`. Holds session state, loaded game, evaluator,
//! and a pending event queue. The app drives it by supplying a per-frame
//! memory peek closure backed by the emulator core.

use crate::http::HttpTransport;

pub struct AchievementRuntime<H: HttpTransport> {
    _http: H,
    _hardcore: bool,
}

impl<H: HttpTransport> AchievementRuntime<H> {
    pub fn new(http: H) -> Self {
        Self {
            _http: http,
            _hardcore: false,
        }
    }
}
