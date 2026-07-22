//! Rust-owned application foundations that are intentionally not connected to Tauri yet.
//!
//! Keeping this module independent from commands and application setup lets the ownership
//! migration land behind unit-tested boundaries before the production cutover.

mod config;
pub(crate) mod contract;
mod core;
pub mod domain;
pub mod error;
pub mod media;
mod pipeline;
pub mod store;
pub mod vad;
pub mod worker;

pub use core::{ApplicationCore, ApplicationCoreConfig};
