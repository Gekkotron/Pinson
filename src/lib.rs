//! House music server: search YouTube Music, keep a "play next" queue,
//! and play it through nidoo-sound.

pub mod api;
pub mod artwork;
pub mod catalog;
pub mod config;
pub mod db;
pub mod engine;
pub mod favorites;
pub mod history;
pub mod import;
pub mod lyrics;
pub mod player;
pub mod playlists;
pub mod queue;
pub mod search;
pub mod song;
pub mod source;
pub mod speakers;
pub mod web;
