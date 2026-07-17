//! Unified HTTP proxy request handler.
//!
//! Merges the previously duplicated `handle_proxy_request` (per-rule) and
//! `handle_proxy_request_shared` (shared listener) into a single function.
//! Route source is selected by the caller via `RouteSource`.

// Concrete implementations arrive in the next tasks.
