//! Rust-owned application core, independent of the Tauri command and event adapters.

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
