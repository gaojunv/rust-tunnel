use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::Level;
use tracing_subscriber::Layer;

use crate::server::db::Database;

/// A log entry captured from tracing events
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Maximum number of log entries kept in the in-memory ring buffer
const MAX_BUFFER_SIZE: usize = 1000;

/// Shared log storage: ring buffer, broadcast channel, DB writer handle
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Mutex<LogStoreInner>>,
    /// Broadcast sender for SSE subscribers
    pub tx: broadcast::Sender<LogEntry>,
    /// Dynamic log level (0=TRACE, 1=DEBUG, 2=INFO, 3=WARN, 4=ERROR)
    pub level: Arc<AtomicU8>,
    /// Buffer write channel — non-blocking send from tracing layer
    buffer_tx: mpsc::UnboundedSender<LogEntry>,
}

struct LogStoreInner {
    buffer: VecDeque<LogEntry>,
    /// Database handle for persistence (None if no DB configured)
    db: Option<Database>,
    /// Batch buffer for periodic DB writes
    db_batch: Vec<LogEntry>,
}

impl LogStore {
    /// Create a new LogStore, optionally backed by a database.
    /// Spawns a background task that drains `buffer_tx` into the ring buffer,
    /// broadcasts to SSE subscribers, and periodically flushes to DB.
    pub fn new(db: Option<Database>) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        let (buffer_tx, mut buffer_rx) = mpsc::unbounded_channel::<LogEntry>();

        let store = Self {
            inner: Arc::new(Mutex::new(LogStoreInner {
                buffer: VecDeque::with_capacity(MAX_BUFFER_SIZE),
                db,
                db_batch: Vec::with_capacity(50),
            })),
            tx,
            level: Arc::new(AtomicU8::new(level_to_u8(Level::INFO))),
            buffer_tx,
        };

        // Spawn background task
        let inner = store.inner.clone();
        let tx = store.tx.clone();
        tokio::spawn(async move {
            let mut db_flush_interval =
                tokio::time::interval(tokio::time::Duration::from_millis(500));

            loop {
                tokio::select! {
                    // Drain incoming log entries
                    maybe_entry = buffer_rx.recv() => {
                        match maybe_entry {
                            Some(entry) => {
                                let mut guard = inner.lock().await;

                                // Push to ring buffer
                                if guard.buffer.len() >= MAX_BUFFER_SIZE {
                                    guard.buffer.pop_front();
                                }
                                guard.buffer.push_back(entry.clone());

                                // Add to DB batch
                                if guard.db.is_some() {
                                    guard.db_batch.push(entry.clone());
                                }

                                drop(guard);

                                // Broadcast to SSE subscribers (non-blocking)
                                let _ = tx.send(entry);
                            }
                            None => break, // channel closed
                        }
                    }
                    // Periodic DB flush
                    _ = db_flush_interval.tick() => {
                        let mut guard = inner.lock().await;
                        if !guard.db_batch.is_empty() {
                            let batch = std::mem::take(&mut guard.db_batch);
                            if let Some(ref db) = guard.db {
                                let db = db.clone();
                                drop(guard);
                                if let Err(e) = db.insert_logs_batch(&batch).await {
                                    tracing::warn!("Failed to flush logs to DB: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        });

        store
    }

    /// Create a LogStore without database persistence
    pub fn new_in_memory() -> Self {
        Self::new(None)
    }

    /// Send a log entry through the non-blocking channel
    pub fn send(&self, entry: LogEntry) {
        let _ = self.buffer_tx.send(entry);
    }

    /// Get all buffered entries (for historical loading)
    pub async fn get_all(&self) -> Vec<LogEntry> {
        let guard = self.inner.lock().await;
        guard.buffer.iter().cloned().collect()
    }

    /// Query buffered entries with in-memory filtering
    pub async fn query(
        &self,
        level: Option<&str>,
        source: Option<&str>,
        search: Option<&str>,
        limit: usize,
    ) -> Vec<LogEntry> {
        let guard = self.inner.lock().await;
        let min_level = match level.map(|l| l.to_lowercase()).as_deref() {
            Some("error") => 4,
            Some("warn") => 3,
            Some("info") => 2,
            Some("debug") => 1,
            Some("trace") => 0,
            _ => 0,
        };

        guard
            .buffer
            .iter()
            .rev()
            .filter(|e| level_to_u8_str(&e.level) >= min_level)
            .filter(|e| {
                if let Some(ref src) = source {
                    e.source.starts_with(src)
                } else {
                    true
                }
            })
            .filter(|e| {
                if let Some(s) = search {
                    e.message.to_lowercase().contains(&s.to_lowercase())
                } else {
                    true
                }
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// Query logs from the database.
    /// Used for pagination (before_id) and when in-memory buffer is insufficient.
    pub async fn query_db(
        &self,
        level: Option<&str>,
        source: Option<&str>,
        search: Option<&str>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Vec<LogEntry> {
        let guard = self.inner.lock().await;
        if let Some(ref db) = guard.db {
            match db.query_logs(level, source, search, limit, before_id).await {
                Ok(rows) => rows
                    .into_iter()
                    .map(|r| LogEntry {
                        id: r.id,
                        timestamp: r.timestamp,
                        level: r.level,
                        source: r.source,
                        target: r.target,
                        message: r.message,
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("Failed to query logs from DB: {}", e);
                    vec![]
                }
            }
        } else {
            vec![]
        }
    }
}

fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    }
}

fn level_to_u8_str(level: &str) -> u8 {
    match level {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" => 3,
        "ERROR" => 4,
        _ => 2, // Default to INFO for unknown levels
    }
}

// ── Custom Tracing Layer ──────────────────────────────────────────

/// A tracing-subscriber Layer that captures events into a LogStore.
/// Only events at or above the LogStore's dynamic level pass through.
pub struct LogLayer {
    store: LogStore,
}

impl LogLayer {
    pub fn new(store: LogStore) -> Self {
        Self { store }
    }
}

impl<S> Layer<S> for LogLayer
where
    S: tracing::Subscriber,
{
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let min_level = self.store.level.load(Ordering::Relaxed);
        let meta_level = level_to_u8(*metadata.level());
        meta_level >= min_level
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);

        // Use the visitor pattern to extract fields
        struct FieldVisitor {
            target: String,
            message: String,
        }

        impl tracing::field::Visit for FieldVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let name = field.name();
                if name == "message" {
                    self.message = format!("{:?}", value);
                    // Strip surrounding quotes if present
                    if self.message.starts_with('"') && self.message.ends_with('"') {
                        self.message = self.message[1..self.message.len() - 1].to_string();
                    }
                } else {
                    // `%field`（display）和裸字段都走这里——必须拼进 message，
                    // 否则日志只剩字面量（"LLM request" 无字段的 bug 根因）
                    if !self.message.is_empty() {
                        self.message.push(' ');
                    }
                    self.message
                        .push_str(&format!("{}={:?}", name, value));
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

        let mut visitor = FieldVisitor {
            target: metadata.target().to_string(),
            message: String::new(),
        };
        event.record(&mut visitor);

        let entry = LogEntry {
            id: 0,
            timestamp,
            level: metadata.level().to_string(),
            source: "server".into(),
            target: visitor.target,
            message: visitor.message,
        };

        self.store.send(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_log_store_new() {
        let store = LogStore::new_in_memory();
        let entries = store.get_all().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_log_store_send_and_get() {
        let store = LogStore::new_in_memory();
        store.send(LogEntry {
            id: 0,
            timestamp: 1000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "hello".into(),
        });

        // Give the background task a moment
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let entries = store.get_all().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "hello");
    }

    #[tokio::test]
    async fn test_log_store_buffer_cap() {
        let store = LogStore::new_in_memory();
        for i in 0..1100 {
            store.send(LogEntry {
                id: 0,
                timestamp: i,
                level: "INFO".into(),
                source: "server".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            });
        }

        // Wait for the background drain task to consume all 1100 entries.
        // Fixed 10ms was flaky under CI load — the task might have only
        // consumed a subset when we call get_all().
        let entries = {
            let mut out = Vec::new();
            for _ in 0..200 {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                out = store.get_all().await;
                if out.len() == 1000 && out[0].timestamp >= 100 {
                    break;
                }
            }
            out
        };

        assert_eq!(entries.len(), 1000);
        // Oldest entries should be dropped
        assert!(entries[0].timestamp >= 100);
    }

    #[tokio::test]
    async fn test_log_store_level_filter() {
        let store = LogStore::new_in_memory();
        store.send(LogEntry {
            id: 0,
            timestamp: 1,
            level: "DEBUG".into(),
            source: "server".into(),
            target: "test".into(),
            message: "debug msg".into(),
        });
        store.send(LogEntry {
            id: 0,
            timestamp: 2,
            level: "ERROR".into(),
            source: "server".into(),
            target: "test".into(),
            message: "error msg".into(),
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let results = store.query(Some("warn"), None, None, 100).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].level, "ERROR");
    }

    #[tokio::test]
    async fn test_log_store_search_filter() {
        let store = LogStore::new_in_memory();
        store.send(LogEntry {
            id: 0,
            timestamp: 1,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "connection established".into(),
        });
        store.send(LogEntry {
            id: 0,
            timestamp: 2,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "connection closed".into(),
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let results = store.query(None, None, Some("established"), 100).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("established"));
    }

    #[tokio::test]
    async fn test_log_store_broadcast() {
        let store = LogStore::new_in_memory();
        let mut rx = store.tx.subscribe();

        store.send(LogEntry {
            id: 0,
            timestamp: 1,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "broadcast test".into(),
        });

        let received = tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.message, "broadcast test");
    }

    #[test]
    fn test_level_conversions() {
        assert_eq!(level_to_u8(tracing::Level::TRACE), 0);
        assert_eq!(level_to_u8(tracing::Level::DEBUG), 1);
        assert_eq!(level_to_u8(tracing::Level::INFO), 2);
        assert_eq!(level_to_u8(tracing::Level::WARN), 3);
        assert_eq!(level_to_u8(tracing::Level::ERROR), 4);

        assert_eq!(level_to_u8_str("TRACE"), 0);
        assert_eq!(level_to_u8_str("DEBUG"), 1);
        assert_eq!(level_to_u8_str("WARN"), 3);
        assert_eq!(level_to_u8_str("UNKNOWN"), 2); // Default to INFO for unknown levels
    }
}
