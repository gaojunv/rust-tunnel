//! 系统通知封装（notify-rust，失败静默）。

/// 发送一条桌面通知；失败时仅 `debug!`，不影响主流程。
pub fn send(summary: &str, body: &str) {
    let r = notify_rust::Notification::new()
        .summary(summary)
        .body(body)
        .show();
    if let Err(e) = r {
        tracing::debug!("notify failed: {e}");
    }
}
