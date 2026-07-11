//! Shared helpers for integration tests.
//!
//! Each test binary does `#[path = "common/mod.rs"] mod common;` at its top.

#![allow(dead_code, unused_imports)] // Different test crates use different subsets.

pub mod api_client;
pub mod echo;
pub mod harness;
pub mod retry;

pub use echo::spawn_echo;
pub use harness::{HarnessOpts, TestHarness};
pub use retry::wait_until;
