use super::AcmeClient;
use std::time::Duration;
use tracing::{error, info};

/// Start the certificate renewal background task
pub fn start_renewal_task(
    client: AcmeClient,
    check_interval_hours: u64,
    days_before_expiry: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(check_interval_hours * 3600));

        loop {
            interval.tick().await;

            info!("Checking for certificates needing renewal...");

            if let Some(db) = client.state.db() {
                match db
                    .load_acme_certificates_needing_renewal(days_before_expiry as i64)
                    .await
                {
                    Ok(certs) => {
                        for cert in certs {
                            info!("Renewing certificate for domain: {}", cert.domain);
                            if let Err(e) = client.renew_certificate(&cert.domain).await {
                                error!("Failed to renew certificate for {}: {}", cert.domain, e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to load certificates for renewal: {}", e);
                    }
                }
            }
        }
    })
}
