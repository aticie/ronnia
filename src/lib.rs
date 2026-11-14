//! Ronnia: the Twitch-to-osu! request relay and account API share one SQLite
//! database and one lifecycle.

pub mod api;
pub mod backup;
pub mod bot;
pub mod config;
pub mod domain;
pub mod parser;
pub mod providers;
pub mod storage;
pub mod twitch_auth;
