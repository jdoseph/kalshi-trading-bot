//! Kalshi-native automated execution engine.
//!
//! Layers (outer depends inward): `market_data`/`orders` -> `client` -> `auth`;
//! `orders` -> `risk`. `auth` and `models` are dependency-free leaves.

pub mod auth;
pub mod client;
pub mod coinbase;
pub mod config;
pub mod market_data;
pub mod models;
pub mod orders;
pub mod risk;
pub mod strategy;
pub mod tui;
pub mod venue;
