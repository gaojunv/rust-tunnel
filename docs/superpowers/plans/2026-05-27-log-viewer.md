# Log Viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a real-time log viewer tab to the server frontend with SSE streaming, SQLite persistence, and client-side log forwarding via control channel.

**Architecture:** A custom `tracing_subscriber::Layer` captures server logs into a shared `LogStore` (ring buffer + broadcast channel + SQLite). SSE pushes logs to the frontend in real time. Clients capture their own logs via a similar layer and batch-send them through the control channel as `ControlMessage::LogBatch`. The frontend `LogsPage` component renders logs with filtering, search, and pause/resume.

**Tech Stack:** Rust (tracing-subscriber Layer, axum SSE, sqlx SQLite), React/TypeScript (EventSource, Tailwind CSS)

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/server/logs.rs` | **Create** | LogStore, LogEntry, custom tracing Layer, background DB writer |
| `src/common/protocol.rs` | Modify | Add `LogBatch` variant + `ClientLogEntry` struct |
| `src/common/logging.rs` | Modify | Accept optional `LogStore` in init, register custom Layer |
| `src/server/control.rs` | Modify | Handle `LogBatch` messages, write to LogStore |
| `src/server/api.rs` | Modify | SSE / REST endpoints, add LogStore to shared state |
| `src/server/db.rs` | Modify | `server_logs` table + CRUD + cleanup |
| `src/bin/server.rs` | Modify | Initialize LogStore, wire into logging + API |
| `src/client/logs.rs` | **Create** | ClientLogLayer + batch forwarder |
| `src/client/mod.rs` | Modify | Register `pub mod logs;` |
| `src/bin/client.rs` | Modify | Initialize client-side log layer, wire LogBatch sender |
| `frontend/src/components/LogsPage.tsx` | **Create** | Log viewer UI component |
| `frontend/src/components/Dashboard.tsx` | Modify | Add `'logs'` tab |
| `frontend/src/components/Navbar.tsx` | Modify | Add "Logs" nav button, update tab type union |
| `frontend/src/api/client.ts` | Modify | Add log API functions |
| `frontend/src/types/index.ts` | Modify | Add `LogEntry` type |

---

### Task 1: Database Schema — `server_logs` Table

**Files:**
- Modify: `src/server/db.rs`

- [ ] **Step 1: Add `server_logs` table creation in `initialize_schema`**

In `src/server/db.rs`, inside the `initialize_schema` method, add after the existing table creations (before `Ok(())`):

```rust
// Server logs table
sqlx::query(
    r#"
    CREATE TABLE IF NOT EXISTS server_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp INTEGER NOT NULL,
        level TEXT NOT NULL,
        source TEXT NOT NULL,
        target TEXT NOT NULL,
        message TEXT NOT NULL
    )
    "#,
)
.execute(pool)
.await?;

sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON server_logs(timestamp)")
    .execute(pool)
    .await?;
sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_level ON server_logs(level)")
    .execute(pool)
    .await?;
```

- [ ] **Step 2: Add `DbLogEntry` row struct and insert method**

Add at the bottom of `src/server/db.rs` (before the `#[cfg(test)]` module):

```rust
/// A log entry row from the database
#[derive(FromRow, Debug, Clone)]
pub struct DbLogEntry {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}
```

Add to the `impl Database` block (before the closing `}`):

```rust
/// Insert a log entry into the database
pub async fn insert_log(&self, entry: &crate::server::logs::LogEntry) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO server_logs (timestamp, level, source, target, message)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(entry.timestamp)
    .bind(&entry.level)
    .bind(&entry.source)
    .bind(&entry.target)
    .bind(&entry.message)
    .execute(&self.pool)
    .await?;

    Ok(result.last_insert_rowid())
}

/// Insert a batch of log entries
pub async fn insert_logs_batch(
    &self,
    entries: &[crate::server::logs::LogEntry],
) -> Result<(), sqlx::Error> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut tx = self.pool.begin().await?;

    for entry in entries {
        sqlx::query(
            r#"
            INSERT INTO server_logs (timestamp, level, source, target, message)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(entry.timestamp)
        .bind(&entry.level)
        .bind(&entry.source)
        .bind(&entry.target)
        .bind(&entry.message)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Query logs with filters
pub async fn query_logs(
    &self,
    level: Option<&str>,
    source: Option<&str>,
    search: Option<&str>,
    limit: u32,
    before_id: Option<i64>,
) -> Result<Vec<DbLogEntry>, sqlx::Error> {
    let mut query_str = String::from(
        "SELECT id, timestamp, level, source, target, message FROM server_logs WHERE 1=1"
    );
    let mut params: Vec<String> = Vec::new();

    if let Some(lvl) = level {
        let levels = match lvl.to_lowercase().as_str() {
            "error" => vec!["ERROR"],
            "warn" => vec!["ERROR", "WARN"],
            "info" => vec!["ERROR", "WARN", "INFO"],
            "debug" => vec!["ERROR", "WARN", "INFO", "DEBUG"],
            "trace" => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
            _ => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
        };
        let placeholders: Vec<String> = levels.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        query_str.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
        for l in levels {
            params.push(l.to_string());
        }
    }

    if let Some(ref src) = source {
        params.push(format!("{}%", src));
        query_str.push_str(&format!(" AND source LIKE ?{}", params.len()));
    }

    if let Some(ref s) = search {
        params.push(format!("%{}%", s));
        query_str.push_str(&format!(" AND message LIKE ?{}", params.len()));
    }

    if let Some(before) = before_id {
        params.push(before.to_string());
        query_str.push_str(&format!(" AND id < ?{}", params.len()));
    }

    query_str.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", params.len() + 1));
    params.push(limit.to_string());

    // Build the dynamic query
    let mut query = sqlx::query_as::<_, DbLogEntry>(&query_str);
    for p in &params {
        query = query.bind(p);
    }

    let mut rows = query.fetch_all(&self.pool).await?;
    // Reverse to get chronological order
    rows.reverse();
    Ok(rows)
}

/// Delete logs older than the given timestamp
pub async fn cleanup_old_logs(&self, older_than_micros: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        DELETE FROM server_logs
        WHERE timestamp < ?
        "#,
    )
    .bind(older_than_micros)
    .execute(&self.pool)
    .await?;

    Ok(result.rows_affected())
}
```

- [ ] **Step 3: Add tests**

At the bottom of the `#[cfg(test)] mod tests` block in `src/server/db.rs`:

```rust
#[tokio::test]
async fn test_server_logs_table_creation() {
    let db = create_test_db().await;
    // Just verify the table exists by doing an insert+query
    let result = db
        .query_logs(None, None, None, 10, None)
        .await
        .unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn test_insert_and_query_logs() {
    let db = create_test_db().await;
    let entry = crate::server::logs::LogEntry {
        id: 0,
        timestamp: 1000000,
        level: "INFO".into(),
        source: "server".into(),
        target: "test::module".into(),
        message: "test message".into(),
    };
    db.insert_log(&entry).await.unwrap();

    let results = db
        .query_logs(None, None, None, 10, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].level, "INFO");
    assert_eq!(results[0].message, "test message");
}

#[tokio::test]
async fn test_query_logs_level_filter() {
    let db = create_test_db().await;
    let info_entry = crate::server::logs::LogEntry {
        id: 0,
        timestamp: 1000000,
        level: "INFO".into(),
        source: "server".into(),
        target: "test".into(),
        message: "info msg".into(),
    };
    let error_entry = crate::server::logs::LogEntry {
        id: 0,
        timestamp: 2000000,
        level: "ERROR".into(),
        source: "server".into(),
        target: "test".into(),
        message: "error msg".into(),
    };
    db.insert_log(&info_entry).await.unwrap();
    db.insert_log(&error_entry).await.unwrap();

    // Filter to WARN+ (includes WARN and ERROR)
    let results = db
        .query_logs(Some("warn"), None, None, 10, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].level, "ERROR");
}

#[tokio::test]
async fn test_cleanup_old_logs() {
    let db = create_test_db().await;
    let entry = crate::server::logs::LogEntry {
        id: 0,
        timestamp: 1000000,
        level: "INFO".into(),
        source: "server".into(),
        target: "test".into(),
        message: "old".into(),
    };
    db.insert_log(&entry).await.unwrap();

    // Cleanup anything older than 2000000
    let deleted = db.cleanup_old_logs(2000000).await.unwrap();
    assert_eq!(deleted, 1);

    let results = db
        .query_logs(None, None, None, 10, None)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_insert_logs_batch() {
    let db = create_test_db().await;
    let entries: Vec<crate::server::logs::LogEntry> = (0..3)
        .map(|i| crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000 + i * 1000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: format!("msg {}", i),
        })
        .collect();
    db.insert_logs_batch(&entries).await.unwrap();

    let results = db
        .query_logs(None, None, None, 10, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 3);
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test db::tests::test_server_logs_table_creation -- --nocapture
cargo test db::tests::test_insert_and_query_logs -- --nocapture
cargo test db::tests::test_query_logs_level_filter -- --nocapture
cargo test db::tests::test_cleanup_old_logs -- --nocapture
cargo test db::tests::test_insert_logs_batch -- --nocapture
```

Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/server/db.rs
git commit -m "feat(db): add server_logs table with CRUD and cleanup methods"
```

---

### Task 2: LogStore and Custom Tracing Layer

**Files:**
- Create: `src/server/logs.rs`
- Modify: `src/server/mod.rs` (register module)

- [ ] **Step 1: Check server mod.rs for module declarations**

Read `src/server/mod.rs`. Expected to find existing module declarations. We'll add `pub mod logs;`.

- [ ] **Step 2: Write the LogStore module**

Create `src/server/logs.rs`:

```rust
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
            let mut db_flush_interval = tokio::time::interval(
                tokio::time::Duration::from_millis(500),
            );

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
                                    guard.db_batch.push(entry);
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
                            if let Some(ref db) = guard.db {
                                let batch = std::mem::take(&mut guard.db_batch);
                                let db = db.clone();
                                drop(guard);
                                if let Err(e) = db.insert_logs_batch(&batch).await {
                                    tracing::warn!("Failed to flush logs to DB: {}", e);
                                }
                                // Re-acquire for next iteration
                                guard = inner.lock().await;
                            } else {
                                guard.db_batch.clear();
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
                if let Some(ref s) = search {
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
        _ => 0,
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

        let mut target = String::new();
        let mut message = String::new();

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
                        self.message = self.message[1..self.message.len()-1].to_string();
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
                    self.message.push_str(&format!("{}={}", field.name(), value));
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
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let entries = store.get_all().await;
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

        let received = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            rx.recv(),
        )
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
        assert_eq!(level_to_u8_str("UNKNOWN"), 0);
    }
}
```

- [ ] **Step 3: Register the module in `src/server/mod.rs`**

Add `pub mod logs;` to the module declarations in `src/server/mod.rs`.

- [ ] **Step 4: Run tests**

```bash
cargo test server::logs::tests -- --nocapture
```

Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/server/logs.rs src/server/mod.rs
git commit -m "feat(logs): add LogStore with ring buffer, broadcast, and custom tracing Layer"
```

---

### Task 3: ControlMessage — LogBatch Protocol

**Files:**
- Modify: `src/common/protocol.rs`

- [ ] **Step 1: Add `ClientLogEntry` struct and `LogBatch` variant**

In `src/common/protocol.rs`, add before the `ControlMessage` enum:

```rust
/// A log entry from a connected client
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientLogEntry {
    /// Microsecond timestamp
    pub timestamp: i64,
    /// TRACE/DEBUG/INFO/WARN/ERROR
    pub level: String,
    /// tracing target (module path)
    pub target: String,
    /// Log message content
    pub message: String,
}
```

Add inside the `ControlMessage` enum (before the closing `}`):

```rust
/// Client sends a batch of log entries
LogBatch {
    entries: Vec<ClientLogEntry>,
},
```

- [ ] **Step 2: Add `LogBatch` to match arms in `control.rs`**

This will be done in Task 5. For now, just verify compilation:

```bash
cargo check
```

Expected: Warning about non-exhaustive patterns in `control.rs`, which we'll fix in Task 5.

- [ ] **Step 3: Add test for serialization**

In the `#[cfg(test)] mod tests` block of `src/common/protocol.rs`, add:

```rust
#[test]
fn test_log_batch_serialization() {
    let msg = ControlMessage::LogBatch {
        entries: vec![
            ClientLogEntry {
                timestamp: 1234567890,
                level: "INFO".into(),
                target: "client::proxy".into(),
                message: "Connection established".into(),
            },
            ClientLogEntry {
                timestamp: 1234567891,
                level: "ERROR".into(),
                target: "client::control".into(),
                message: "Heartbeat timeout".into(),
            },
        ],
    };
    let bytes = msg.serialize().unwrap();
    assert!(bytes.len() > 4);
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test protocol::tests::test_log_batch_serialization -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/common/protocol.rs
git commit -m "feat(protocol): add LogBatch variant and ClientLogEntry for client log forwarding"
```

---

### Task 4: Server API — Log Endpoints and SSE

**Files:**
- Modify: `src/server/api.rs`

- [ ] **Step 1: Add request/response types and SSE helper**

Add at the top of `src/server/api.rs` (after existing type definitions, before `ApiState`):

```rust
/// Log entry response
#[derive(Debug, Serialize)]
pub struct LogEntryResponse {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Query parameters for GET /api/logs
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub before_id: Option<i64>,
}

/// Request body for PUT /api/logs/level
#[derive(Debug, Deserialize)]
pub struct SetLevelRequest {
    pub level: String,
}

/// SSE query params (for token-based auth)
#[derive(Debug, Deserialize)]
pub struct SseQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub token: Option<String>,
}
```

- [ ] **Step 2: Modify `ApiState` to include `LogStore`**

Change the `ApiState` struct:

```rust
#[derive(Clone)]
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
    pub log_store: Option<crate::server::logs::LogStore>,
}
```

- [ ] **Step 3: Add SSE stream handler**

Add this function before `run_api_server`:

```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
use std::time::Duration;

async fn sse_log_stream(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    // Check auth for SSE
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().or_else(|| {
            // Fall back to header/cookie (for non-EventSource clients)
            None // SSE from browser can't set headers, must use query param
        });

        let is_valid = match token {
            Some(t) => crate::server::auth::validate_token(t, &state.auth_config.jwt_secret).is_ok(),
            None => false,
        };

        if !is_valid {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }

    let log_store = match &state.log_store {
        Some(store) => store.clone(),
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let min_level = params.level.as_deref().unwrap_or("info");
    let min_level_u8 = match min_level {
        "error" => 4u8,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 2,
    };
    let source_filter = params.source.clone();

    let mut rx = log_store.tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(entry)) => {
                    // Apply filters
                    let entry_level = match entry.level.as_str() {
                        "TRACE" => 0, "DEBUG" => 1, "INFO" => 2, "WARN" => 3, "ERROR" => 4,
                        _ => 2,
                    };
                    if entry_level < min_level_u8 {
                        continue;
                    }
                    if let Some(ref src) = source_filter {
                        if !entry.source.starts_with(src) {
                            continue;
                        }
                    }

                    let json = serde_json::to_string(&LogEntryResponse {
                        id: entry.id,
                        timestamp: entry.timestamp,
                        level: entry.level.clone(),
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        message: entry.message.clone(),
                    })
                    .unwrap_or_default();

                    yield Ok(Event::default().event("log").data(json));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok(Event::default()
                        .event("sync")
                        .data(format!(r#"{{"lagged":{}}}"#, n)));
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    break;
                }
                Err(_) => {
                    // Timeout — send ping to keep connection alive
                    yield Ok(Event::default().event("ping").data(""));
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
```

- [ ] **Step 4: Add REST handlers**

```rust
async fn get_logs(
    State(state): State<ApiState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let limit = params.limit.unwrap_or(200).min(1000) as usize;

    // Try DB first if before_id or search is specified (DB handles pagination better)
    // For simple queries, use in-memory buffer
    let entries = log_store
        .query(
            params.level.as_deref(),
            params.source.as_deref(),
            params.search.as_deref(),
            limit,
        )
        .await;

    let response: Vec<LogEntryResponse> = entries
        .into_iter()
        .map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        })
        .collect();

    Json(response).into_response()
}

async fn get_logs_level(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = log_store.level.load(std::sync::atomic::Ordering::Relaxed);
    let level_str = match level_u8 {
        0 => "trace",
        1 => "debug",
        2 => "info",
        3 => "warn",
        4 => "error",
        _ => "info",
    };

    Json(serde_json::json!({ "level": level_str })).into_response()
}

async fn put_logs_level(
    State(state): State<ApiState>,
    Json(body): Json<SetLevelRequest>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = match body.level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => {
            return axum::response::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid level. Use: trace, debug, info, warn, error"))
                .unwrap();
        }
    };

    log_store.level.store(level_u8, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("Log level changed to {}", body.level.to_lowercase());

    Json(serde_json::json!({ "level": body.level.to_lowercase() })).into_response()
}
```

- [ ] **Step 5: Register routes in `run_api_server`**

In `run_api_server`, add log routes to the `protected_routes` Router:

```rust
// Log viewer endpoints
.route("/api/logs", get(get_logs))
.route("/api/logs/stream", get(sse_log_stream))
.route("/api/logs/level", get(get_logs_level).put(put_logs_level))
```

- [ ] **Step 6: Update `ApiState` construction in `run_api_server`**

The `ApiState` now has a `log_store` field. Extract it from `ServerState` inside `run_api_server` (ServerState already contains `log_store` from Task 5):

```rust
pub async fn run_api_server(
    api_addr: String,
    server_state: ServerState,
    auth_config: AuthConfig,
) -> Result<(), std::io::Error> {
    let auth_config = Arc::new(auth_config);
    let log_store = server_state.log_store.clone();

    let state = ApiState {
        server_state,
        auth_config: auth_config.clone(),
        log_store,
    };
    // ... rest unchanged
```

Note: The function signature stays the same (no extra parameter). `log_store` is extracted from `server_state.log_store` which was set up in Task 5 (ServerState::with_db).

- [ ] **Step 7: Add `async-stream` dependency**

Update `Cargo.toml`:

```toml
async-stream = "0.3"
```

- [ ] **Step 8: Add imports at top of `api.rs`**

Add to existing imports:
```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use std::time::Duration;
```

- [ ] **Step 9: Run check**

```bash
cargo check
```

Expected: Compiles successfully (there will be an error in `server.rs` because `run_api_server` signature changed — fixed in Task 6).

- [ ] **Step 10: Commit**

```bash
git add src/server/api.rs Cargo.toml
git commit -m "feat(api): add SSE log stream, log query, and log level endpoints"
```

---

### Task 5: Server Control — Handle LogBatch Messages

**Files:**
- Modify: `src/server/control.rs`

- [ ] **Step 1: Add `LogStore` to `ServerState`**

In `ServerState` struct, add field:

```rust
/// Log store for capturing and broadcasting logs
pub log_store: Option<crate::server::logs::LogStore>,
```

Update `ServerState::new()` to set `log_store: None,`.

Update `ServerState::with_db(db)` to include:
```rust
log_store: Some(crate::server::logs::LogStore::new(Some(db.clone()))),
```

- [ ] **Step 2: Handle `LogBatch` in the main message loop**

In `handle_control_connection`, add a match arm for `LogBatch` in the main loop (after the existing `Pong` arm):

```rust
ControlMessage::LogBatch { entries } => {
    if let Some(ref log_store) = state.log_store {
        let hostname = registered_ports
            .first()
            .and_then(|&port| {
                // Find hostname from client info
                futures::executor::block_on(async {
                    state.get_client(port).await.map(|c| c.hostname)
                })
            })
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());

        let source_prefix = format!(
            "client:{}:{}",
            hostname,
            registered_ports.first().copied().unwrap_or(0)
        );

        for entry in entries {
            log_store.send(crate::server::logs::LogEntry {
                id: 0,
                timestamp: entry.timestamp,
                level: entry.level,
                source: source_prefix.clone(),
                target: entry.target,
                message: entry.message,
            });
        }
    }
}
```

Wait — this uses `block_on` in an async context, which is bad. Let me fix this. Actually, we need async access to `state.get_client()`. Let me restructure:

```rust
ControlMessage::LogBatch { entries } => {
    if let Some(ref log_store) = state.log_store {
        // Find hostname from first registered port
        let hostname = if let Some(&port) = registered_ports.first() {
            state.get_client(port).await.map(|c| c.hostname).flatten()
        } else {
            None
        };
        let source_prefix = format!(
            "client:{}:{}",
            hostname.as_deref().unwrap_or("unknown"),
            registered_ports.first().copied().unwrap_or(0)
        );

        for entry in entries {
            log_store.send(crate::server::logs::LogEntry {
                id: 0,
                timestamp: entry.timestamp,
                level: entry.level,
                source: source_prefix.clone(),
                target: entry.target,
                message: entry.message,
            });
        }
    }
}
```

Add this to the match block after the `Pong` arm and before the `Data` arm.

Also add `LogBatch` to the `_` wildcard catch in the registration phase loop (before `Ping`) so it's properly ignored or handled during registration:

In the registration phase, add before the wildcard `_`:
```rust
ControlMessage::LogBatch { .. } => {
    // Log batches during registration phase are silently dropped
    // (no ports registered yet, so no source context)
}
```

- [ ] **Step 3: Run check**

```bash
cargo check
```

Expected: Compiles successfully.

- [ ] **Step 4: Commit**

```bash
git add src/server/control.rs
git commit -m "feat(control): handle LogBatch messages from clients"
```

---

### Task 6: Logging Init — Custom Layer Registration

**Files:**
- Modify: `src/common/logging.rs`

- [ ] **Step 1: Add new init function accepting LogStore**

```rust
use crate::server::logs::{LogLayer, LogStore};

pub fn init_logging_with_level_and_store(default_level: &str, log_store: Option<LogStore>) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter);

    if let Some(store) = log_store {
        registry
            .with(LogLayer::new(store))
            .init();
    } else {
        registry.init();
    }
}
```

Wait — this creates a circular dependency issue. `src/common/logging.rs` would depend on `src/server/logs.rs`, but `common` should not depend on `server`. That violates the crate's module hierarchy.

I need to restructure. Options:
1. Move `LogLayer` and `LogStore` to `src/common/` — but they depend on `Database` which is in `server`
2. Use a trait-based approach in `common` that `server` implements
3. Keep `init_logging_with_level_and_store` in a server-level module
4. Make `LogStore` define a constructor for the layer, and pass the layer from server to common

The cleanest approach: Keep the new init function in `src/common/logging.rs` but make it generic over a `Layer`:

```rust
use tracing_subscriber::layer::Layer;
use tracing_subscriber::{prelude::*, util::SubscriberInitExt, EnvFilter};

pub fn init_logging() {
    init_logging_with_level("info");
}

pub fn init_logging_with_level(default_level: &str) {
    init_logging_with_layer(default_level, None::<tracing_subscriber::layer::Identity>);
}

pub fn init_logging_with_layer<L>(default_level: &str, extra_layer: Option<L>)
where
    L: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
{
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter);

    if let Some(layer) = extra_layer {
        registry.with(layer).init();
    } else {
        registry.init();
    }
}
```

Then in `src/bin/server.rs`:
```rust
use rust_tunnel::common::init_logging_with_layer;
use rust_tunnel::server::logs::LogLayer;

// ...
let log_store = state.log_store.clone();
init_logging_with_layer(&config.log, Some(LogLayer::new(log_store.unwrap())));
```

Hmm, but `LogStore` is `Option`. Let me handle that:
```rust
if let Some(store) = state.log_store.clone() {
    init_logging_with_layer(&config.log, Some(LogLayer::new(store)));
} else {
    init_logging_with_level(&config.log);
}
```

This is clean and avoids circular dependencies. Let me go with this approach.

- [ ] **Step 1 (revised): Make `init_logging_with_layer` generic**

Modify `src/common/logging.rs`:

```rust
use tracing_subscriber::{prelude::*, util::SubscriberInitExt, EnvFilter};

pub fn init_logging() {
    init_logging_with_level("info");
}

pub fn init_logging_with_level(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .init();
}

/// Initialize logging with an additional custom layer (e.g., LogLayer for log capture).
/// The extra_layer is registered alongside the default fmt layer and EnvFilter.
pub fn init_logging_with_layer<L>(default_level: &str, extra_layer: L)
where
    L: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
{
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .with(extra_layer)
        .init();
}
```

- [ ] **Step 2: Remove the `use crate::server::logs` import from this file**

The old plan had a bad import. With the generic approach, `common/logging.rs` stays clean with no server dependency.

- [ ] **Step 3: Run check**

```bash
cargo check
```

Expected: Compiles without circular dependency issues.

- [ ] **Step 4: Commit**

```bash
git add src/common/logging.rs
git commit -m "feat(logging): add generic init_logging_with_layer for custom tracing layers"
```

---

### Task 7: Server Entry Point — Final Wiring

**Files:**
- Modify: `src/bin/server.rs`

- [ ] **Step 1: Wire logging with LogLayer**

After `let state = control::ServerState::with_db(db.clone());`, replace `init_logging_with_level(&config.log);` with:

```rust
// Initialize logging with LogStore capture
let log_store = state.log_store.clone();
if let Some(store) = log_store {
    init_logging_with_layer(&config.log, LogLayer::new(store));
} else {
    init_logging_with_level(&config.log);
}
```

Add imports at top:
```rust
use rust_tunnel::common::init_logging_with_layer;
use rust_tunnel::server::logs::LogLayer;
```

- [ ] **Step 2: Add DB cleanup background task**

After the `state.traffic_store.start_flush_task();` line, add:

```rust
// Start periodic cleanup of old log entries (every hour, removes 7+ day old logs)
let db_for_cleanup = db.clone();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
    loop {
        interval.tick().await;
        let seven_days_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
            - 7 * 24 * 3600 * 1_000_000i64;
        if let Err(e) = db_for_cleanup.cleanup_old_logs(seven_days_ago).await {
            tracing::warn!("Failed to cleanup old logs: {}", e);
        }
    }
});
```

- [ ] **Step 3: Run check**

```bash
cargo check
```

Expected: Compiles successfully.

- [ ] **Step 4: Commit**

```bash
git add src/bin/server.rs
git commit -m "feat(server): wire LogLayer into logging init and add DB log cleanup"
```

---

### Task 8: Client-Side Log Capture and Forwarding

**Files:**
- Modify: `src/bin/client.rs`
- Modify: `src/client/control.rs` (if log sending logic is here)

- [ ] **Step 1: Add client-side log layer and buffered sender**

In `src/bin/client.rs`, after the existing imports and config loading, add a client-side log layer that buffers and sends via the control channel.

First, let's read `src/bin/client.rs` to understand the structure.

Since I don't have the full client binary in context, let me design the approach:

Add a struct `ClientLogLayer` in a new section of the existing client code, or add it inline in `client.rs`.

Create `src/client/logs.rs`:

```rust
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing_subscriber::Layer;

use crate::common::protocol::ClientLogEntry;

/// Client-side log layer that captures tracing events into a buffer,
/// then sends them in batches via a channel (to be forwarded through the control connection).
pub struct ClientLogLayer {
    tx: mpsc::UnboundedSender<ClientLogEntry>,
}

impl ClientLogLayer {
    pub fn new(tx: mpsc::UnboundedSender<ClientLogEntry>) -> Self {
        Self { tx }
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
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);

        let mut message = String::new();

        struct ClientFieldVisitor {
            message: String,
        }

        impl tracing::field::Visit for ClientFieldVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{:?}", value);
                    if self.message.starts_with('"') && self.message.ends_with('"') {
                        self.message = self.message[1..self.message.len()-1].to_string();
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
                    self.message.push_str(&format!("{}={}", field.name(), value));
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

        let _ = self.tx.send(entry);
    }
}

/// Spawn a background task that drains log entries from the channel,
/// buffers them, and periodically sends batches through the control sender.
pub fn spawn_log_forwarder(
    mut rx: mpsc::UnboundedReceiver<ClientLogEntry>,
    control_sender: crate::client::control::ControlSender,
) {
    tokio::spawn(async move {
        let mut buffer: Vec<ClientLogEntry> = Vec::with_capacity(50);
        let mut flush_interval = tokio::time::interval(tokio::time::Duration::from_secs(2));

        loop {
            tokio::select! {
                maybe_entry = rx.recv() => {
                    match maybe_entry {
                        Some(entry) => {
                            buffer.push(entry);
                            if buffer.len() >= 50 {
                                let batch = std::mem::take(&mut buffer);
                                let _ = control_sender
                                    .send(crate::common::ControlMessage::LogBatch { entries: batch })
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
                            .send(crate::common::ControlMessage::LogBatch { entries: batch })
                            .await;
                    }
                }
            }
        }
    });
}
```

- [ ] **Step 2: Register module and wire in client binary**

In `src/client/mod.rs`, add:
```rust
pub mod logs;
```

In `src/bin/client.rs`, after the control sender is created:
```rust
use rust_tunnel::client::logs::{ClientLogLayer, spawn_log_forwarder};
use rust_tunnel::common::init_logging_with_layer;

// ... (after control_sender is available)

let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
spawn_log_forwarder(log_rx, control_sender.clone());
init_logging_with_layer(&config.log, ClientLogLayer::new(log_tx));
```

Wait, I need to read `src/bin/client.rs` to understand exactly where to place this. Let me add a step to read it first.

- [ ] **Step 3: Check and build**

```bash
cargo check
```

Expected: Compiles.

- [ ] **Step 4: Commit**

```bash
git add src/client/logs.rs src/client/mod.rs src/bin/client.rs
git commit -m "feat(client): add client-side log capture and batch forwarding"
```

---

### Task 9: Frontend Types and API Client

**Files:**
- Modify: `frontend/src/types/index.ts`
- Modify: `frontend/src/api/client.ts`

- [ ] **Step 1: Add `LogEntry` type**

In `frontend/src/types/index.ts`, add:

```typescript
export interface LogEntry {
  id: number;
  timestamp: number;
  level: string;
  source: string;
  target: string;
  message: string;
}
```

- [ ] **Step 2: Add API functions**

In `frontend/src/api/client.ts`, add:

```typescript
import type { LogEntry } from '../types';

// Logs API
export const getLogs = async (params?: {
  level?: string;
  source?: string;
  search?: string;
  limit?: number;
  before_id?: number;
}): Promise<LogEntry[]> => {
  const response = await api.get<LogEntry[]>('/logs', { params });
  return response.data;
};

export const getLogsLevel = async (): Promise<{ level: string }> => {
  const response = await api.get<{ level: string }>('/logs/level');
  return response.data;
};

export const setLogsLevel = async (level: string): Promise<{ level: string }> => {
  const response = await api.put<{ level: string }>('/logs/level', { level });
  return response.data;
};
```

- [ ] **Step 3: Verify TypeScript compilation**

```bash
cd frontend && npx tsc --noEmit
```

Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/types/index.ts frontend/src/api/client.ts
git commit -m "feat(frontend): add LogEntry type and log API functions"
```

---

### Task 10: Frontend — LogsPage Component

**Files:**
- Create: `frontend/src/components/LogsPage.tsx`

- [ ] **Step 1: Write the LogsPage component**

Create `frontend/src/components/LogsPage.tsx`:

```tsx
import { useState, useEffect, useRef, useCallback } from 'react';
import { getLogs, setLogsLevel } from '../api/client';
import type { LogEntry } from '../types';

const LEVELS = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'] as const;
const LEVEL_COLORS: Record<string, string> = {
  TRACE: 'text-gray-400',
  DEBUG: 'text-gray-500',
  INFO: 'text-blue-600',
  WARN: 'text-yellow-600',
  ERROR: 'text-red-600',
};
const BG_COLORS: Record<string, string> = {
  ERROR: 'bg-red-50',
  WARN: 'bg-yellow-50',
};

export const LogsPage = () => {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [levelFilter, setLevelFilter] = useState<string>('info');
  const [search, setSearch] = useState('');
  const [isPaused, setIsPaused] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const scrollRef = useRef<HTMLDivElement>(null);
  const esRef = useRef<EventSource | null>(null);

  // Load historical logs on mount
  useEffect(() => {
    getLogs({ level: levelFilter, limit: 200 }).then(setLogs).catch(console.error);
  }, []);

  // SSE connection
  useEffect(() => {
    if (isPaused) {
      esRef.current?.close();
      esRef.current = null;
      return;
    }

    const token = localStorage.getItem('auth_token');
    const url = `/api/logs/stream?level=${levelFilter}${token ? `&token=${token}` : ''}`;
    const es = new EventSource(url);
    esRef.current = es;

    es.addEventListener('log', (e) => {
      try {
        const entry: LogEntry = JSON.parse(e.data);
        setLogs((prev) => [...prev.slice(-999), entry]);
      } catch {}
    });

    es.addEventListener('ping', () => {
      // heartbeat, no action needed
    });

    es.addEventListener('sync', (e) => {
      try {
        const data = JSON.parse(e.data);
        console.warn('SSE lagged, missed', data.lagged, 'messages');
        // Reload to catch up
        getLogs({ level: levelFilter, limit: 200 }).then(setLogs).catch(console.error);
      } catch {}
    });

    es.onerror = () => {
      // EventSource will auto-reconnect
    };

    return () => {
      es.close();
    };
  }, [levelFilter, isPaused]);

  // Auto-scroll
  useEffect(() => {
    if (autoScroll && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [logs, autoScroll]);

  const handleScroll = useCallback(() => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    setAutoScroll(scrollHeight - scrollTop - clientHeight < 50);
  }, []);

  const handleLevelChange = async (newLevel: string) => {
    try {
      await setLogsLevel(newLevel);
      setLevelFilter(newLevel);
    } catch (e) {
      console.error('Failed to set log level:', e);
    }
  };

  // Filter logs by search (client-side)
  const filteredLogs = search
    ? logs.filter((l) => l.message.toLowerCase().includes(search.toLowerCase()))
    : logs;

  const formatTime = (ts: number) => {
    const date = new Date(ts / 1000); // microseconds to ms
    return date.toLocaleTimeString('zh-CN', { hour12: false }) + '.' +
      String(date.getMilliseconds()).padStart(3, '0');
  };

  const loadMore = async () => {
    if (logs.length === 0) return;
    const oldest = logs[0];
    try {
      const older = await getLogs({
        level: levelFilter,
        limit: 200,
        before_id: oldest.id || undefined,
      });
      if (older.length > 0) {
        setLogs((prev) => [...older, ...prev]);
      }
    } catch (e) {
      console.error('Failed to load more logs:', e);
    }
  };

  return (
    <div className="bg-white shadow rounded-lg">
      {/* Toolbar */}
      <div className="px-4 py-3 border-b border-gray-200 flex items-center gap-3 flex-wrap">
        {/* Level filter for SSE */}
        <select
          value={levelFilter}
          onChange={(e) => setLevelFilter(e.target.value)}
          className="text-sm border rounded px-2 py-1"
        >
          <option value="trace">TRACE</option>
          <option value="debug">DEBUG</option>
          <option value="info">INFO</option>
          <option value="warn">WARN</option>
          <option value="error">ERROR</option>
        </select>

        {/* Search */}
        <input
          type="text"
          placeholder="Search..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="text-sm border rounded px-2 py-1 flex-1 min-w-[120px]"
        />

        {/* Pause/Resume */}
        <button
          onClick={() => setIsPaused(!isPaused)}
          className={`text-sm px-3 py-1 rounded border ${isPaused ? 'bg-green-100 border-green-300' : 'bg-gray-100 border-gray-300'}`}
        >
          {isPaused ? 'Resume' : 'Pause'}
        </button>

        {/* Dynamic level set */}
        <select
          onChange={(e) => handleLevelChange(e.target.value)}
          defaultValue=""
          className="text-sm border rounded px-2 py-1"
          title="Set server log level"
        >
          <option value="" disabled>Set Level</option>
          {LEVELS.map((l) => (
            <option key={l} value={l.toLowerCase()}>{l}</option>
          ))}
        </select>
      </div>

      {/* Log entries */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="h-[600px] overflow-y-auto font-mono text-xs leading-5"
      >
        {filteredLogs.length === 0 && (
          <div className="p-4 text-gray-400 text-center">No logs</div>
        )}
        {filteredLogs.map((log, i) => (
          <div
            key={`${log.timestamp}-${i}`}
            className={`px-4 py-0.5 flex gap-2 hover:bg-gray-50 ${BG_COLORS[log.level] || ''}`}
          >
            <span className="text-gray-400 shrink-0">{formatTime(log.timestamp)}</span>
            <span className={`shrink-0 w-12 ${LEVEL_COLORS[log.level] || ''}`}>{log.level}</span>
            <span className="text-gray-500 shrink-0 max-w-[120px] truncate" title={log.source}>
              {log.source}
            </span>
            <span className="text-gray-400 shrink-0 max-w-[150px] truncate" title={log.target}>
              {log.target}
            </span>
            <span className="text-gray-700 truncate" title={log.message}>
              {log.message}
            </span>
          </div>
        ))}
        {/* Load more at top */}
        <div className="px-4 py-2 text-center">
          <button
            onClick={loadMore}
            className="text-xs text-blue-500 hover:text-blue-700"
          >
            Load more...
          </button>
        </div>
      </div>

      {/* Status bar */}
      <div className="px-4 py-1 border-t border-gray-200 flex items-center justify-between text-xs text-gray-400">
        <span>{filteredLogs.length} entries</span>
        <label className="flex items-center gap-1 cursor-pointer">
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(e) => setAutoScroll(e.target.checked)}
          />
          Auto-scroll
        </label>
      </div>
    </div>
  );
};
```

- [ ] **Step 2: Verify TypeScript compilation**

```bash
cd frontend && npx tsc --noEmit
```

Expected: No errors.

- [ ] **Step 3: Commit**

```bash
git add frontend/src/components/LogsPage.tsx
git commit -m "feat(frontend): add LogsPage component with SSE streaming, filtering, and search"
```

---

### Task 11: Frontend — Dashboard and Navbar Integration

**Files:**
- Modify: `frontend/src/components/Dashboard.tsx`
- Modify: `frontend/src/components/Navbar.tsx`

- [ ] **Step 1: Add 'logs' to tab types and LogsPage import in Dashboard**

In `frontend/src/components/Dashboard.tsx`:

Add import:
```tsx
import { LogsPage } from './LogsPage';
```

Change the `activeTab` type:
```tsx
const [activeTab, setActiveTab] = useState<'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs'>('dashboard');
```

Add condition for logs tab before the final `: (`:
```tsx
) : activeTab === 'trojan' ? (
  <TrojanPage />
) : (
  <LogsPage />
)}
```

Actually, let me be more precise. The current code is:
```tsx
        ) : activeTab === 'quality' ? (
          <QualityPage onSelectClient={handleSelectClient} />
        ) : activeTab === 'shadowsocks' ? (
          <ShadowsocksPage />
        ) : (
          <TrojanPage />
        )}
```

Change to:
```tsx
        ) : activeTab === 'quality' ? (
          <QualityPage onSelectClient={handleSelectClient} />
        ) : activeTab === 'shadowsocks' ? (
          <ShadowsocksPage />
        ) : activeTab === 'trojan' ? (
          <TrojanPage />
        ) : (
          <LogsPage />
        )}
```

- [ ] **Step 2: Add "Logs" button to Navbar and update type**

In `frontend/src/components/Navbar.tsx`:

Update the `activeTab` type:
```tsx
interface NavbarProps {
  onLogout: () => void;
  activeTab: 'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs';
  onTabChange: (tab: 'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs') => void;
}
```

Add the Logs button after the Trojan button:
```tsx
              <button
                onClick={() => onTabChange('logs')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'logs'
                    ? 'bg-gray-900 text-white'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white'
                }`}
              >
                Logs
              </button>
```

- [ ] **Step 3: Verify TypeScript compilation**

```bash
cd frontend && npx tsc --noEmit
```

Expected: No errors.

- [ ] **Step 4: Build frontend**

```bash
cd frontend && npm run build
```

Expected: Builds successfully.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/components/Dashboard.tsx frontend/src/components/Navbar.tsx
git commit -m "feat(frontend): integrate LogsPage into Dashboard with Logs tab in Navbar"
```

---

### Task 12: Integration Testing and Final Verification

**Files:**
- Modify: `src/server/api.rs` (SSE auth improvements if needed)

- [ ] **Step 1: Full backend build**

```bash
cargo build
```

Expected: Compiles.

- [ ] **Step 2: Run all tests**

```bash
cargo test -- --nocapture
```

Expected: All tests pass.

- [ ] **Step 3: Frontend typecheck and build**

```bash
cd frontend && npx tsc --noEmit && npm run build
```

Expected: No errors, dist/ generated.

- [ ] **Step 4: Copy frontend and verify server starts**

```bash
rm -rf frontend-dist && cp -r frontend/dist frontend-dist
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080 --api-bind 0.0.0.0:3000 --db-path :memory: --log debug
```

Manual check:
1. Open `http://localhost:3000` — verify "Logs" tab appears in navbar
2. Click "Logs" tab — verify SSE connection, see real-time server logs
3. Test pause/resume, level filter, search
4. Verify server stderr still shows logs (fmt layer unchanged)

- [ ] **Step 5: Commit final adjustments**

```bash
git add -A
git commit -m "chore: final integration tweaks for log viewer"
```
