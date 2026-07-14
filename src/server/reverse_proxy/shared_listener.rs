//! Shared HTTP listener implementation (Task 9 fills this in).

use std::collections::HashSet;
use std::sync::Arc;
use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::router::RouteTable;

pub struct SharedListener {
    pub listen_addr: String,
    pub tls_enabled: bool,
    pub route_table: Arc<ArcSwap<RouteTable>>,
    pub active_rule_ids: HashSet<String>,
    pub(crate) handle: JoinHandle<()>,
    pub(crate) shutdown_tx: watch::Sender<bool>,
}
