#![doc = include_str!("../README.md")]

pub mod api;
pub mod client;
pub mod condition;
pub mod evaluator;
pub mod events;
pub mod hash;
pub mod http;
pub mod leaderboard;
pub mod memaddr;
pub mod patch;
pub mod richpresence;

pub use client::AchievementRuntime;
pub use events::AchievementEvent;
pub use http::HttpTransport;
#[cfg(feature = "http-fetch")]
pub use http::FetchTransport;
