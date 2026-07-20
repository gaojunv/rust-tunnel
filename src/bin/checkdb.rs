use rust_tunnel::server::db::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::new("./data/rust-tunnel.db").await?;

    // Diagnose the unified stats_snapshots table (last 24 hours)
    let now = chrono::Utc::now();
    let rows = db
        .query_stats_snapshots(&[], &[], now - chrono::Duration::hours(24), now)
        .await?;

    // Group snapshot counts by (entity_type, entity_id)
    let mut counts: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    for row in &rows {
        *counts
            .entry((row.entity_type.clone(), row.entity_id.clone()))
            .or_insert(0) += 1;
    }

    println!("stats_snapshots (last 24h): {} rows", rows.len());
    for ((entity_type, entity_id), n) in &counts {
        println!("  {entity_type}/{entity_id}: {n} snapshots");
    }

    Ok(())
}
