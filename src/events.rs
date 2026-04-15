//! Public event enum emitted by `AchievementRuntime`.

pub enum AchievementEvent {
    Unlocked {
        id: u32,
        title: String,
        points: u32,
        icon_url: String,
    },
    ChallengeShow(u32),
    ChallengeHide(u32),
    ProgressUpdate {
        id: u32,
        measured: String,
    },
    LeaderboardStarted(u32),
    LeaderboardCancelled(u32),
    LeaderboardSubmitted {
        id: u32,
        score: i64,
    },
    ServerError(String),
}
