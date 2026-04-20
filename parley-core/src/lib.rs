//! Parley shared data layer.
//!
//! This crate is the home for types and pure logic that need to be visible
//! to both the web frontend (the root `parley` crate, compiled to WASM) and
//! the native `parley-proxy` (which owns filesystem, networking, and the
//! conversation orchestrator). Anything in here MUST compile under
//! `wasm32-unknown-unknown` — no `std::fs`, no `tokio`, no networking.
//!
//! See `docs/architecture.md` for the broader system shape.

pub mod chat;
pub mod model_config;
pub mod persona;
pub mod speaker;
pub mod word_graph;
