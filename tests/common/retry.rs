//! Exponential-backoff polling helper used by every integration test.
//!
//! Never `sleep(2s)` — always `wait_until(cond).await`.

use std::future::Future;
use std::time::Duration;

/// Poll `cond` up to 50 times with exponential backoff (base 20ms, cap 500ms).
/// Total worst-case wait ≈ 5 seconds.
///
/// Returns `Ok(T)` on the first `Some(T)` from `cond`, `Err(String)` after exhaustion.
pub async fn wait_until<F, Fut, T>(desc: &str, mut cond: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let mut delay = Duration::from_millis(20);
    for attempt in 0..50 {
        if let Some(value) = cond().await {
            return Ok(value);
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_millis(500));
        let _ = attempt;
    }
    Err(format!("wait_until timed out after 50 attempts: {desc}"))
}
