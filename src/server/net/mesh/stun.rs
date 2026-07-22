use crate::common::{ATTR_XOR_MAPPED_ADDRESS, STUN_MAGIC_COOKIE};
use std::net::SocketAddr;
use tokio::net::UdpSocket;

/// Lightweight STUN server that responds to Binding Requests
/// with the client's observed public address (XOR-MAPPED-ADDRESS).
pub struct StunServer {
    socket: UdpSocket,
}

impl StunServer {
    pub async fn bind(addr: &str) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self { socket })
    }

    /// Run the STUN server loop. Does not return.
    pub async fn run(self) -> anyhow::Result<()> {
        let mut buf = [0u8; 1500];
        loop {
            let (len, src_addr) = self.socket.recv_from(&mut buf).await?;
            let response = Self::handle_binding_request(&buf[..len], src_addr);
            if let Some(resp) = response {
                self.socket.send_to(&resp, src_addr).await?;
            }
        }
    }

    fn handle_binding_request(data: &[u8], src_addr: SocketAddr) -> Option<Vec<u8>> {
        if data.len() < 20 {
            return None;
        }

        // Only handle Binding Requests (type 0x0001)
        let msg_type = u16::from_be_bytes([data[0], data[1]]);
        if msg_type != 0x0001 {
            return None;
        }

        let transaction_id = &data[8..20];

        // Build XOR-MAPPED-ADDRESS attribute
        let mut attr = Vec::new();
        match src_addr {
            SocketAddr::V4(v4) => {
                let ip = u32::from_be_bytes(v4.ip().octets());
                let port = v4.port();

                // XOR with magic cookie per RFC 5389 Section 15.2
                let xor_port = port ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
                let xor_ip = ip ^ STUN_MAGIC_COOKIE;

                attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes()); // type
                attr.extend_from_slice(&8u16.to_be_bytes()); // length
                attr.push(0x00); // reserved
                attr.push(0x01); // IPv4 family
                attr.extend_from_slice(&xor_port.to_be_bytes());
                attr.extend_from_slice(&xor_ip.to_be_bytes());
            }
            SocketAddr::V6(_) => {
                // IPv6 not supported in lightweight implementation
                return None;
            }
        }

        let mut response = Vec::with_capacity(20 + attr.len());
        // Message type: Binding Success Response (0x0101)
        response.extend_from_slice(&0x0101u16.to_be_bytes());
        // Message length
        response.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        // Magic cookie
        response.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        // Transaction ID (copied from request)
        response.extend_from_slice(transaction_id);
        // Attributes
        response.extend_from_slice(&attr);

        Some(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::build_binding_request;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn test_handle_binding_request_valid() {
        let (request, tid) = build_binding_request();
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 12345));

        let response = StunServer::handle_binding_request(&request, src).unwrap();
        assert!(response.len() > 20);

        // Verify it's a Binding Response (0x0101)
        let msg_type = u16::from_be_bytes([response[0], response[1]]);
        assert_eq!(msg_type, 0x0101);

        // Verify transaction ID is echoed back
        let resp_tid = &response[8..20];
        assert_eq!(resp_tid, &tid as &[u8]);
    }

    #[test]
    fn test_handle_binding_request_empty() {
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 12345));
        let response = StunServer::handle_binding_request(&[], src);
        assert!(response.is_none());
    }

    #[test]
    fn test_handle_binding_request_wrong_type() {
        // Build a packet with a non-Binding-Request type
        let mut packet = [0u8; 20];
        packet[0] = 0x01; // Not a Binding Request
        packet[1] = 0x01;
        packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 12345));
        let response = StunServer::handle_binding_request(&packet, src);
        assert!(response.is_none());
    }
}
