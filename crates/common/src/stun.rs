use rand::Rng;
use serde::{Deserialize, Serialize};

/// STUN message types (RFC 5389)
pub const STUN_BINDING_REQUEST: u16 = 0x0001;
pub const STUN_BINDING_RESPONSE: u16 = 0x0101;
pub const STUN_MAGIC_COOKIE: u32 = 0x2112A442;

/// MAPPED-ADDRESS attribute (RFC 5389 Section 15.1)
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// XOR-MAPPED-ADDRESS attribute (RFC 5389 Section 15.2)
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Parsed STUN message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StunMessage {
    pub msg_type: u16,
    pub transaction_id: [u8; 12],
    /// Parsed mapped address as "ip:port"
    pub mapped_address: Option<String>,
}

/// Build a STUN Binding Request packet.
/// Returns the raw packet bytes and the transaction ID for verification.
#[must_use] 
pub fn build_binding_request() -> ([u8; 20], [u8; 12]) {
    let mut rng = rand::thread_rng();
    let mut transaction_id = [0u8; 12];
    rng.fill(&mut transaction_id);

    let mut packet = [0u8; 20];
    // Message type: Binding Request (0x0001)
    packet[0] = 0x00;
    packet[1] = 0x01;
    // Message length: 0 (no attributes)
    packet[2] = 0x00;
    packet[3] = 0x00;
    // Magic cookie
    packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    // Transaction ID
    packet[8..20].copy_from_slice(&transaction_id);

    (packet, transaction_id)
}

/// Parse a STUN Binding Response and extract the XOR-MAPPED-ADDRESS.
/// Returns None if the message is malformed, wrong type, or TID mismatch.
#[must_use] 
pub fn parse_binding_response(data: &[u8], expected_tid: &[u8; 12]) -> Option<StunMessage> {
    if data.len() < 20 {
        return None;
    }

    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != STUN_BINDING_RESPONSE {
        return None;
    }

    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < 20 + msg_len {
        return None;
    }

    // Verify magic cookie
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if magic != STUN_MAGIC_COOKIE {
        return None;
    }

    // Verify transaction ID
    let mut tid = [0u8; 12];
    tid.copy_from_slice(&data[8..20]);
    if tid != *expected_tid {
        return None;
    }

    // Parse attributes
    let mut mapped_address = None;
    let mut pos = 20;
    while pos + 4 <= data.len() && pos + 4 <= 20 + msg_len {
        let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + attr_len > data.len() {
            break;
        }

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS if attr_len >= 8 => {
                if let Some(addr) = parse_xor_mapped_address(&data[pos..pos + attr_len]) {
                    mapped_address = Some(addr);
                }
            }
            ATTR_MAPPED_ADDRESS if mapped_address.is_none() && attr_len >= 8 => {
                if let Some(addr) = parse_mapped_address(&data[pos..pos + attr_len]) {
                    mapped_address = Some(addr);
                }
            }
            _ => {}
        }

        // Align to 4-byte boundary
        pos += (attr_len + 3) & !3;
    }

    Some(StunMessage {
        msg_type,
        transaction_id: tid,
        mapped_address,
    })
}

fn parse_xor_mapped_address(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    // Skip first byte (reserved), second byte is family (0x01 = IPv4)
    let family = data[1];
    if family != 0x01 {
        return None; // Only IPv4 supported
    }
    let port_xor = u16::from_be_bytes([data[2], data[3]]);
    let port = port_xor ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
    let ip_xor = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ip = ip_xor ^ STUN_MAGIC_COOKIE;
    let ip_str = format!(
        "{}.{}.{}.{}",
        (ip >> 24) & 0xFF,
        (ip >> 16) & 0xFF,
        (ip >> 8) & 0xFF,
        ip & 0xFF
    );
    Some(format!("{ip_str}:{port}"))
}

fn parse_mapped_address(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    let family = data[1];
    if family != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([data[2], data[3]]);
    let ip = format!("{}.{}.{}.{}", data[4], data[5], data[6], data[7]);
    Some(format!("{ip}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_binding_request() {
        let (packet, tid) = build_binding_request();
        assert_eq!(packet.len(), 20);
        assert_eq!(tid.len(), 12);
        assert_eq!(packet[0], 0x00);
        assert_eq!(packet[1], 0x01);
        let magic = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        assert_eq!(magic, STUN_MAGIC_COOKIE);
    }

    #[test]
    fn test_parse_binding_response_empty() {
        let result = parse_binding_response(&[], &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_binding_response_short() {
        let result = parse_binding_response(&[0u8; 10], &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_binding_response_wrong_type() {
        let mut packet = [0u8; 20];
        packet[0] = 0x00;
        packet[1] = 0x01; // Binding Request, not Response
        packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        let result = parse_binding_response(&packet, &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_binding_request_unique_tids() {
        let (_, tid1) = build_binding_request();
        let (_, tid2) = build_binding_request();
        assert_ne!(tid1, tid2);
    }

    #[test]
    fn test_parse_xor_mapped_address_roundtrip() {
        // Build a valid XOR-MAPPED-ADDRESS attribute for 1.2.3.4:12345
        let ip: u32 = 0x01020304; // 1.2.3.4
        let port: u16 = 12345;
        let xor_ip = ip ^ STUN_MAGIC_COOKIE;
        let xor_port = port ^ ((STUN_MAGIC_COOKIE >> 16) as u16);

        let mut attr = Vec::new();
        attr.push(0x00); // reserved
        attr.push(0x01); // IPv4
        attr.extend_from_slice(&xor_port.to_be_bytes());
        attr.extend_from_slice(&xor_ip.to_be_bytes());

        let result = parse_xor_mapped_address(&attr);
        assert_eq!(result, Some("1.2.3.4:12345".to_string()));
    }

    #[test]
    fn test_parse_mapped_address_roundtrip() {
        let mut attr = Vec::new();
        attr.push(0x00); // reserved
        attr.push(0x01); // IPv4
        attr.extend_from_slice(&12345u16.to_be_bytes());
        attr.extend_from_slice(&[1, 2, 3, 4]);

        let result = parse_mapped_address(&attr);
        assert_eq!(result, Some("1.2.3.4:12345".to_string()));
    }
}
