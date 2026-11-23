//! House music server: search YouTube Music, keep a "play next" queue,
//! and play it on speakers.

pub mod api;
pub mod artwork;
pub mod autoplay;
pub mod catalog;
pub mod config;
pub mod cover_color;
pub mod crossfade;
pub mod db;
pub mod engine;
pub mod fade;
pub mod favorites;
pub mod fuzzy;
pub mod history;
pub mod import;
pub mod loudness;
pub mod lyrics;
pub mod mixes;
pub mod moments;
pub mod moods;
pub mod mqtt;
pub mod offline;
pub mod output;
pub mod player;
pub mod playlists;
pub mod queue;
pub mod radio;
pub mod search;
pub mod song;
pub mod source;
pub mod speakers;
pub mod stats;
pub mod web;
