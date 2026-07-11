//! Shared helpers for integration tests.
//!
//! Each test binary does `#[path = "common/mod.rs"] mod common;` at its top.

#![allow(dead_code)] // Different test crates use different subsets.

pub mod api_client;
pub mod echo;
pub mod harness;
pub mod retry;

pub use api_client::ApiClient;
pub use echo::{spawn_echo, spawn_http_echo};
pub use harness::{HarnessOpts, TestHarness};
pub use retry::wait_until;
