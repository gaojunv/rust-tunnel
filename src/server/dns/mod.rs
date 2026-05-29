pub mod registry;
pub mod zone;

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tracing::{debug, error, info};

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::{BinEncodable, BinEncoder};

use self::registry::DnsRegistry;

/// Lightweight authoritative DNS server for tunnel.local and mesh.local zones.
pub struct DnsServer {
    registry: DnsRegistry,
    bind_addr: SocketAddr,
}

impl DnsServer {
    pub fn new(registry: DnsRegistry, bind_addr: &str) -> Result<Self, String> {
        let addr: SocketAddr = bind_addr
            .parse()
            .map_err(|e| format!("Invalid DNS bind address '{}': {}", bind_addr, e))?;
        Ok(Self {
            registry,
            bind_addr: addr,
        })
    }

    /// Start the DNS server, listening on UDP. Does not return.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let socket = UdpSocket::bind(self.bind_addr).await?;
        info!("DNS server listening on {}", self.bind_addr);

        let registry = self.registry;
        let mut buf = vec![0u8; 512];

        loop {
            let (len, src_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    error!("DNS recv error: {}", e);
                    continue;
                }
            };

            let request_data = buf[..len].to_vec();
            let registry = registry.clone();

            // Process each query in a spawned task
            let socket_clone = socket.local_addr().ok();
            tokio::spawn(async move {
                let response = handle_dns_query(&registry, &request_data).await;
                // Re-bind is wasteful but tokio UdpSocket doesn't easily share across tasks.
                // For a lightweight DNS server, this is acceptable.
                if let Ok(send_socket) = UdpSocket::bind("0.0.0.0:0").await {
                    if let Err(e) = send_socket.send_to(&response, src_addr).await {
                        error!("DNS send error to {}: {}", src_addr, e);
                    }
                }
                let _ = socket_clone;
            });
        }
    }
}

async fn handle_dns_query(registry: &DnsRegistry, data: &[u8]) -> Vec<u8> {
    let request = match Message::from_vec(data) {
        Ok(req) => req,
        Err(e) => {
            debug!("Failed to parse DNS query: {}", e);
            return build_error_response(0, ResponseCode::FormErr);
        }
    };

    let req_id = request.id();

    // Only handle standard queries
    if request.message_type() != MessageType::Query || request.op_code() != OpCode::Query {
        return build_error_response(req_id, ResponseCode::NotImp);
    }

    let mut response = Message::new();
    response.set_id(req_id);
    response.set_message_type(MessageType::Response);
    response.set_op_code(OpCode::Query);
    response.set_authoritative(true);
    response.set_recursion_available(false);

    if request.query_count() == 0 {
        response.set_response_code(ResponseCode::FormErr);
        return encode_message(&response);
    }

    let mut response_code = ResponseCode::NoError;

    for question in request.queries() {
        let qname = question.name().to_string();
        let qname = qname.trim_end_matches('.').to_lowercase();

        debug!("DNS query: {} type={:?}", qname, question.query_type());

        match question.query_type() {
            RecordType::A => {
                let ips = registry.query_a(&qname).await;
                if ips.is_empty() {
                    response_code = ResponseCode::NXDomain;
                } else {
                    let mut name = question.name().clone();
                    name.set_fqdn(false);
                    for ip in &ips {
                        if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                            let mut record = Record::new();
                            record.set_name(name.clone());
                            record.set_record_type(RecordType::A);
                            record.set_ttl(300);
                            record.set_data(Some(RData::A(hickory_proto::rr::rdata::A(addr))));
                            response.add_answer(record);
                        }
                    }
                }
            }
            RecordType::SRV => {
                let srvs = registry.query_srv(&qname).await;
                if srvs.is_empty() {
                    response_code = ResponseCode::NXDomain;
                } else {
                    for (target, port) in &srvs {
                        if let Ok(target_name) = Name::from_ascii(&format!("{}.", target)) {
                            let mut record = Record::new();
                            record.set_name(question.name().clone());
                            record.set_record_type(RecordType::SRV);
                            record.set_ttl(300);
                            record.set_data(Some(RData::SRV(hickory_proto::rr::rdata::SRV::new(
                                0,
                                0,
                                *port,
                                target_name,
                            ))));
                            response.add_answer(record);
                        }
                    }
                }
            }
            _ => {
                response_code = ResponseCode::NXDomain;
            }
        }
    }

    response.set_response_code(response_code);
    encode_message(&response)
}

fn encode_message(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut encoder = BinEncoder::new(&mut buf);
    if msg.emit(&mut encoder).is_err() {
        return build_error_response(msg.id(), ResponseCode::ServFail);
    }
    let bytes = encoder.into_bytes();
    // Truncate to 512 bytes max for UDP DNS
    if bytes.len() > 512 {
        bytes[..512].to_vec()
    } else {
        bytes.to_vec()
    }
}

fn build_error_response(id: u16, code: ResponseCode) -> Vec<u8> {
    let mut response = Message::new();
    response.set_id(id);
    response.set_message_type(MessageType::Response);
    response.set_response_code(code);
    encode_message(&response)
}
