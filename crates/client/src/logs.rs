use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing_subscriber::Layer;

use crate::control::ControlSender;
use rust_tunnel_common::protocol::ClientLogEntry;
use rust_tunnel_common::ControlMessage;

/// 客户端日志批量上报刷新间隔：2s，或满 50 条立即刷。
const LOG_FLUSH_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(2);

/// A tracing [`Layer`] that captures log events on the client and forwards them
/// through an mpsc channel so they can be batched and sent to the server.
///
/// The inner sender is wrapped in `Arc<Mutex<Option<...>>>` so it can be
/// hot-swapped across reconnections: each time the client reconnects it
/// creates a fresh channel pair and calls [`set_sender`](ClientLogLayer::set_sender).
#[derive(Clone)]
pub struct ClientLogLayer {
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<ClientLogEntry>>>>,
}

impl ClientLogLayer {
    /// Create a layer with no sender yet.  Call [`set_sender`](Self::set_sender)
    /// before (or after) registering this layer with `tracing_subscriber`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Replace the inner sender (e.g. after a reconnection).
    pub fn set_sender(&self, sender: mpsc::UnboundedSender<ClientLogEntry>) {
        if let Ok(mut guard) = self.tx.lock() {
            *guard = Some(sender);
        }
    }
}

impl Default for ClientLogLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for ClientLogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as i64);

        struct ClientFieldVisitor {
            message: String,
        }

        impl tracing::field::Visit for ClientFieldVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                    if self.message.starts_with('"') && self.message.ends_with('"') {
                        self.message = self.message[1..self.message.len() - 1].to_string();
                    }
                }
            }

            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    if !self.message.is_empty() {
                        self.message.push(' ');
                    }
                    self.message
                        .push_str(&format!("{}={}", field.name(), value));
                }
            }
        }

        let mut visitor = ClientFieldVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);

        let entry = ClientLogEntry {
            timestamp,
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message,
        };

        if let Ok(guard) = self.tx.lock() {
            if let Some(ref tx) = *guard {
                let _ = tx.send(entry);
            }
        }
    }
}

/// Spawn a background task that buffers log entries and sends them as a
/// [`ControlMessage::LogBatch`] to the server.
///
/// The buffer is flushed:
/// - when it reaches 50 entries, or
/// - every 2 seconds (whichever comes first).
pub fn spawn_log_forwarder(
    mut rx: mpsc::UnboundedReceiver<ClientLogEntry>,
    control_sender: ControlSender,
) {
    tokio::spawn(async move {
        let mut buffer: Vec<ClientLogEntry> = Vec::with_capacity(50);
        let mut flush_interval = tokio::time::interval(LOG_FLUSH_INTERVAL);

        loop {
            tokio::select! {
                maybe_entry = rx.recv() => {
                    match maybe_entry {
                        Some(entry) => {
                            buffer.push(entry);
                            if buffer.len() >= 50 {
                                let batch = std::mem::take(&mut buffer);
                                let _ = control_sender
                                    .send(ControlMessage::LogBatch { entries: batch })
                                    .await;
                            }
                        }
                        None => break,
                    }
                }
                _ = flush_interval.tick() => {
                    if !buffer.is_empty() {
                        let batch = std::mem::take(&mut buffer);
                        let _ = control_sender
                            .send(ControlMessage::LogBatch { entries: batch })
                            .await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    /// Verify that the layer captures a simple info event and that the entry
    /// arrives on the receiver side.
    #[test]
    fn test_client_log_layer_captures_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let layer = ClientLogLayer::new();
        layer.set_sender(tx);

        let _guard = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(layer)
            .set_default();

        tracing::info!(target: "test_target", "hello world");

        let entry = rx.try_recv().expect("expected a log entry");
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.target, "test_target");
        assert_eq!(entry.message, "hello world");
        assert!(entry.timestamp > 0);
    }

    /// Verify that the layer captures the message and any structured fields.
    #[test]
    fn test_client_log_layer_with_structured_fields() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let layer = ClientLogLayer::new();
        layer.set_sender(tx);

        let _guard = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(layer)
            .set_default();

        tracing::info!(foo = "bar", "hello");

        let entry = rx.try_recv().expect("expected a log entry");
        assert!(entry.message.contains("hello"));
        assert!(entry.message.contains("foo=bar"));
    }

    /// Verify that the forwarder flushes at least one batch within a reasonable
    /// time (it uses a 2-second interval, so we wait up to 3 seconds).
    #[tokio::test]
    async fn test_spawn_log_forwarder_flushes_on_interval() {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(32);

        spawn_log_forwarder(log_rx, control_tx);

        // Send a single entry -- buffer is not full, so it will wait for the
        // 2-second timer.
        log_tx
            .send(ClientLogEntry {
                timestamp: 1000,
                level: "INFO".into(),
                target: "test".into(),
                message: "flush me".into(),
            })
            .ok();

        // The flush interval fires every 2 s, so we should receive a batch
        // within 3 s.
        let timeout = tokio::time::timeout(Duration::from_secs(3), control_rx.recv()).await;
        match timeout {
            Ok(Some(ControlMessage::LogBatch { entries })) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].message, "flush me");
            }
            other => panic!("expected Some(LogBatch) within timeout, got {other:?}"),
        }
    }

    /// Verify that the forwarder flushes immediately when buffer reaches 50.
    #[tokio::test]
    async fn test_spawn_log_forwarder_flushes_on_full_buffer() {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(32);

        spawn_log_forwarder(log_rx, control_tx);

        // Send 50 entries -- should trigger an immediate flush.
        for i in 0..50 {
            log_tx
                .send(ClientLogEntry {
                    timestamp: i64::from(i),
                    level: "INFO".into(),
                    target: "test".into(),
                    message: format!("entry {i}"),
                })
                .ok();
        }

        let timeout = tokio::time::timeout(Duration::from_secs(1), control_rx.recv()).await;
        match timeout {
            Ok(Some(ControlMessage::LogBatch { entries })) => {
                assert_eq!(entries.len(), 50);
            }
            other => panic!("expected Some(LogBatch) within timeout, got {other:?}"),
        }
    }

    use std::time::Duration;

    /// Verify that set_sender can replace the sender and new events go to the
    /// new receiver.
    #[test]
    fn test_client_log_layer_replace_sender() {
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let layer = ClientLogLayer::new();
        layer.set_sender(tx1);

        let _guard = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(layer.clone())
            .set_default();

        tracing::info!("first");

        let entry = rx1.try_recv().expect("expected entry on rx1");
        assert_eq!(entry.message, "first");

        // Replace sender
        layer.set_sender(tx2);

        tracing::info!("second");

        // rx1 should not receive the second event
        assert!(rx1.try_recv().is_err());

        // rx2 should receive it
        let entry = rx2.try_recv().expect("expected entry on rx2");
        assert_eq!(entry.message, "second");
    }
}
