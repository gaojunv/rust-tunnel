use rust_tunnel::server::db::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::new("./data/rust-tunnel.db").await?;
    
    // Check total rows
    let ports = db.get_quality_ports(24).await?;
    println!("Ports found (last 24h): {:?}", ports);
    
    for port in ports {
        let history = db.get_quality_history(port, chrono::Utc::now() - chrono::Duration::hours(24), chrono::Utc::now()).await?;
        println!("Port {}: {} samples", port, history.len());
        for (i, s) in history.iter().take(3).enumerate() {
            println!("  {}: {}ms, score={}", i, s.avg_rtt_ms, s.quality_score);
        }
    }
    
    Ok(())
}
