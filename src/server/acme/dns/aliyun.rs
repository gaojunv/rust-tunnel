use async_trait::async_trait;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Aliyun DNS API base URL
const ALIYUN_DNS_API: &str = "https://alidns.aliyuncs.com/";

/// Parse a domain into the root domain and RR (host record) for Aliyun DNS API.
///
/// - `"test.example.com"` -> `("example.com", "test")`
/// - `"example.com"`      -> `("example.com", "@")`
/// - `"*.example.com"`    -> `("example.com", "*")`
fn parse_domain(domain: &str) -> anyhow::Result<(String, String)> {
    // Strip wildcard prefix if present
    let clean_domain = domain.strip_prefix("*.").unwrap_or(domain);

    let parts: Vec<&str> = clean_domain.split('.').collect();

    if parts.len() < 2 {
        return Err(anyhow::anyhow!("Invalid domain format: {}", domain));
    }

    // Root domain is the last two parts (e.g., "example.com")
    let main_domain = parts[parts.len() - 2..].join(".");

    // Determine the RR (host record)
    let rr = if domain.starts_with("*.") {
        "*".to_string()
    } else if parts.len() > 2 {
        parts[..parts.len() - 2].join(".")
    } else {
        "@".to_string()
    };

    Ok((main_domain, rr))
}

/// Aliyun DNS challenge solver
pub struct AliyunDnsSolver {
    access_key_id: String,
    access_key_secret: String,
    client: reqwest::Client,
}

impl AliyunDnsSolver {
    /// Create a new Aliyun DNS solver
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            access_key_id: config.api_key.clone(),
            access_key_secret: config.api_secret.clone().unwrap_or_default(),
            client: reqwest::Client::new(),
        }
    }

    /// Generate Aliyun API signature parameters using HMAC-SHA1
    fn sign_request(&self, params: &mut Vec<(String, String)>) {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha1::Sha1;

        // Add common parameters
        params.push(("Format".to_string(), "JSON".to_string()));
        params.push(("Version".to_string(), "2015-01-09".to_string()));
        params.push(("AccessKeyId".to_string(), self.access_key_id.clone()));
        params.push(("SignatureMethod".to_string(), "HMAC-SHA1".to_string()));
        params.push((
            "Timestamp".to_string(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ));
        params.push(("SignatureVersion".to_string(), "1.0".to_string()));
        params.push((
            "SignatureNonce".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ));

        // Sort parameters by key
        params.sort_by(|a, b| a.0.cmp(&b.0));

        // Build canonical query string
        let query_string: String = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    urlencoding::encode(k),
                    urlencoding::encode(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&");

        // Build string to sign
        let string_to_sign = format!(
            "GET&{}&{}",
            urlencoding::encode("/"),
            urlencoding::encode(&query_string)
        );

        // Compute HMAC-SHA1 signature
        type HmacSha1 = Hmac<Sha1>;
        let mut mac =
            HmacSha1::new_from_slice(format!("{}&", self.access_key_secret).as_bytes())
                .expect("HMAC can take key of any size");
        mac.update(string_to_sign.as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        // Append signature
        params.push(("Signature".to_string(), signature));
    }

    /// Call the Aliyun DNS API with the given action and extra parameters
    async fn call_api(
        &self,
        action: &str,
        extra_params: Vec<(String, String)>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = vec![("Action".to_string(), action.to_string())];
        params.extend(extra_params);

        self.sign_request(&mut params);

        let query_string: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}?{}", ALIYUN_DNS_API, query_string);

        debug!("Calling Aliyun API: action={}", action);

        let response = self.client.get(&url).send().await?;

        let status = response.status();
        let body: serde_json::Value = response.json().await?;

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Aliyun API error: {} - {}",
                status,
                body
            ));
        }

        // Check for Aliyun business-level errors
        if let Some(error_code) = body.get("Code") {
            return Err(anyhow::anyhow!(
                "Aliyun API error: {} - {}",
                error_code,
                body.get("Message")
                    .unwrap_or(&serde_json::Value::String("Unknown error".to_string()))
            ));
        }

        Ok(body)
    }

    /// Find an existing TXT record by domain and value
    async fn find_txt_record(
        &self,
        domain: &str,
        value: &str,
    ) -> anyhow::Result<Option<String>> {
        let (main_domain, rr) = parse_domain(domain)?;

        let params = vec![
            ("DomainName".to_string(), main_domain.clone()),
            ("RR".to_string(), rr.clone()),
            ("Type".to_string(), "TXT".to_string()),
        ];

        let body = self.call_api("DescribeDomainRecords", params).await?;

        if let Some(records) = body.get("DomainRecords").and_then(|r| r.get("Record")) {
            if let Some(arr) = records.as_array() {
                for record in arr {
                    if let (Some(record_value), Some(record_id)) =
                        (record.get("Value"), record.get("RecordId"))
                    {
                        if record_value.as_str() == Some(value) {
                            if let Some(id) = record_id.as_str() {
                                return Ok(Some(id.to_string()));
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

#[async_trait]
impl DnsChallengeSolver for AliyunDnsSolver {
    async fn create_txt_record(&self, domain: &str, value: &str) -> anyhow::Result<()> {
        let (main_domain, rr) = parse_domain(domain)?;

        info!(
            "Creating Aliyun DNS TXT record: {}.{} = {}",
            rr, main_domain, value
        );

        let params = vec![
            ("DomainName".to_string(), main_domain),
            ("RR".to_string(), rr),
            ("Type".to_string(), "TXT".to_string()),
            ("Value".to_string(), value.to_string()),
            ("TTL".to_string(), "600".to_string()),
        ];

        let body = self.call_api("AddDomainRecord", params).await?;

        if let Some(record_id) = body.get("RecordId").and_then(|r| r.as_str()) {
            info!("Created Aliyun DNS TXT record, RecordId: {}", record_id);
        }

        Ok(())
    }

    async fn delete_txt_record(&self, domain: &str, value: &str) -> anyhow::Result<()> {
        let (_main_domain, _rr) = parse_domain(domain)?;

        // Find the existing record
        let record_id = match self.find_txt_record(domain, value).await? {
            Some(id) => id,
            None => {
                warn!("No matching Aliyun DNS TXT record found for domain {}", domain);
                return Ok(());
            }
        };

        info!(
            "Deleting Aliyun DNS TXT record: RecordId={}",
            record_id
        );

        let params = vec![("RecordId".to_string(), record_id)];

        let _body = self.call_api("DeleteDomainRecord", params).await?;

        info!("Deleted Aliyun DNS TXT record successfully");
        Ok(())
    }

    async fn wait_for_propagation(
        &self,
        _domain: &str,
        _value: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        // Wait a fixed interval for DNS propagation
        let wait = std::cmp::min(timeout, Duration::from_secs(30));
        debug!("Waiting {:?} for Aliyun DNS propagation", wait);
        tokio::time::sleep(wait).await;
        Ok(())
    }

    fn provider_name(&self) -> &str {
        "aliyun"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_domain_subdomain() {
        let (domain, rr) = parse_domain("test.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "test");
    }

    #[test]
    fn test_parse_domain_bare() {
        let (domain, rr) = parse_domain("example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "@");
    }

    #[test]
    fn test_parse_domain_wildcard() {
        let (domain, rr) = parse_domain("*.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "*");
    }

    #[test]
    fn test_parse_domain_deep_subdomain() {
        let (domain, rr) = parse_domain("a.b.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "a.b");
    }

    #[test]
    fn test_parse_domain_invalid() {
        assert!(parse_domain("com").is_err());
    }

    #[test]
    fn test_parse_acme_challenge_domain() {
        // For wildcard domain *.example.com, the ACME challenge domain
        // should be _acme-challenge.example.com
        let domain = "_acme-challenge.example.com";
        let (main_domain, rr) = parse_domain(domain).unwrap();
        assert_eq!(main_domain, "example.com");
        assert_eq!(rr, "_acme-challenge");
    }

    #[test]
    fn test_dns_txt_value_calculation() {
        // DNS-01 challenge TXT record value calculation:
        // key_authorization = token + "." + thumbprint(account_key)
        // txt_value = base64url(sha256(key_authorization))

        use base64::Engine;
        use sha2::{Digest, Sha256};

        let key_authorization = "test_token.test_thumbprint";
        let mut hasher = Sha256::new();
        hasher.update(key_authorization.as_bytes());
        let hash = hasher.finalize();
        let txt_value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

        // Verify the result is non-empty
        assert!(!txt_value.is_empty());
        // Verify it's a valid base64url string
        assert!(txt_value
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }
}
