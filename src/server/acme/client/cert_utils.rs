use anyhow::{Context, Result};
use x509_parser::prelude::*;

/// Parse a PEM certificate chain and extract the expiry date of the first (leaf) certificate
pub(super) fn parse_certificate_expiry(cert_chain_pem: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    // Parse the PEM data
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_chain_pem.as_bytes())
        .context("Failed to parse certificate PEM")?;

    // Parse the DER-encoded certificate
    let (_, cert) =
        X509Certificate::from_der(&pem.contents).context("Failed to parse certificate DER")?;

    // Extract the expiry date (x509-parser returns time::OffsetDateTime, convert to chrono)
    let not_after = cert.validity.not_after.to_datetime();
    let ts = not_after.unix_timestamp();
    let naive = chrono::DateTime::from_timestamp(ts, 0)
        .context("Failed to create DateTime from timestamp")?;

    Ok(naive)
}

/// Split a PEM certificate chain into the leaf certificate and the remaining chain
pub(super) fn split_certificate_chain(cert_chain_pem: &str) -> (String, String) {
    let mut certs = Vec::new();
    let mut current_cert = String::new();
    let mut in_cert = false;

    for line in cert_chain_pem.lines() {
        if line.contains("-----BEGIN CERTIFICATE-----") {
            in_cert = true;
            current_cert.clear();
            current_cert.push_str(line);
            current_cert.push('\n');
        } else if line.contains("-----END CERTIFICATE-----") {
            current_cert.push_str(line);
            current_cert.push('\n');
            certs.push(current_cert.clone());
            current_cert.clear();
            in_cert = false;
        } else if in_cert {
            current_cert.push_str(line);
            current_cert.push('\n');
        }
    }

    match certs.len() {
        0 => (cert_chain_pem.to_string(), String::new()),
        1 => (certs[0].clone(), String::new()),
        _ => {
            let leaf = certs[0].clone();
            let chain: String = certs[1..].join("");
            (leaf, chain)
        }
    }
}