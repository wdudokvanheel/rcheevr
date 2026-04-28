//! Public event enum emitted by `AchievementRuntime`.

use serde::ser::{Serialize, SerializeMap, Serializer};

#[derive(Debug)]
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

/// Tagged JSON shape (`{ "type": "...", ... }`) consumed by the web frontend
/// and the iOS app. The field name `image_url` (rather than `icon_url`) is
/// kept for compatibility with existing JS consumers.
impl Serialize for AchievementEvent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unlocked {
                id,
                title,
                points,
                icon_url,
            } => {
                let mut m = s.serialize_map(Some(5))?;
                m.serialize_entry("type", "unlocked")?;
                m.serialize_entry("id", id)?;
                m.serialize_entry("title", title)?;
                m.serialize_entry("points", points)?;
                m.serialize_entry("image_url", icon_url)?;
                m.end()
            }
            Self::ChallengeShow(id) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "challenge_show")?;
                m.serialize_entry("id", id)?;
                m.end()
            }
            Self::ChallengeHide(id) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "challenge_hide")?;
                m.serialize_entry("id", id)?;
                m.end()
            }
            Self::ProgressUpdate { id, measured } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", "progress")?;
                m.serialize_entry("id", id)?;
                m.serialize_entry("measured", measured)?;
                m.end()
            }
            Self::LeaderboardStarted(id) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "leaderboard_started")?;
                m.serialize_entry("id", id)?;
                m.end()
            }
            Self::LeaderboardCancelled(id) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "leaderboard_cancelled")?;
                m.serialize_entry("id", id)?;
                m.end()
            }
            Self::LeaderboardSubmitted { id, score } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", "leaderboard_submitted")?;
                m.serialize_entry("id", id)?;
                m.serialize_entry("score", score)?;
                m.end()
            }
            Self::ServerError(message) => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", "server_error")?;
                m.serialize_entry("message", message)?;
                m.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlocked_serializes_with_image_url_alias() {
        let ev = AchievementEvent::Unlocked {
            id: 42,
            title: "Got Pokédex".to_string(),
            points: 5,
            icon_url: "https://example/badge.png".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "unlocked");
        assert_eq!(v["id"], 42);
        assert_eq!(v["title"], "Got Pokédex");
        assert_eq!(v["points"], 5);
        assert_eq!(v["image_url"], "https://example/badge.png");
        assert!(v.get("icon_url").is_none());
    }

    #[test]
    fn leaderboard_started_serializes_as_tagged_object() {
        let ev = AchievementEvent::LeaderboardStarted(7);
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "leaderboard_started");
        assert_eq!(v["id"], 7);
    }

    #[test]
    fn leaderboard_submitted_includes_score() {
        let ev = AchievementEvent::LeaderboardSubmitted { id: 7, score: 42 };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "leaderboard_submitted");
        assert_eq!(v["id"], 7);
        assert_eq!(v["score"], 42);
    }

    #[test]
    fn server_error_uses_message_field() {
        let ev = AchievementEvent::ServerError("nope".to_string());
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "server_error");
        assert_eq!(v["message"], "nope");
    }

    #[test]
    fn progress_update_uses_progress_tag() {
        let ev = AchievementEvent::ProgressUpdate {
            id: 1,
            measured: "5/10".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "progress");
        assert_eq!(v["id"], 1);
        assert_eq!(v["measured"], "5/10");
    }

    #[test]
    fn challenge_show_hide_distinct_tags() {
        let show: serde_json::Value =
            serde_json::to_value(AchievementEvent::ChallengeShow(3)).unwrap();
        let hide: serde_json::Value =
            serde_json::to_value(AchievementEvent::ChallengeHide(3)).unwrap();
        assert_eq!(show["type"], "challenge_show");
        assert_eq!(hide["type"], "challenge_hide");
        assert_eq!(show["id"], 3);
        assert_eq!(hide["id"], 3);
    }

    #[test]
    fn vec_of_events_round_trips_to_json_array() {
        let events = vec![
            AchievementEvent::LeaderboardStarted(1),
            AchievementEvent::ServerError("x".to_string()),
        ];
        let s = serde_json::to_string(&events).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.is_array());
        assert_eq!(v[0]["type"], "leaderboard_started");
        assert_eq!(v[1]["type"], "server_error");
    }
}
