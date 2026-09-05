//! 轻量权威 DNS 服务：tunnel/mesh 域解析与 UDP 服务。

/// DNS 注册表。
pub mod registry;
/// DNS 区域内存表。
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
    /// 创建 DNS 服务端。
    ///
    /// # Errors
    /// 当 `bind_addr` 无法解析为 `SocketAddr` 时返回错误。
    pub fn new(registry: DnsRegistry, bind_addr: &str) -> Result<Self, String> {
        let addr: SocketAddr = bind_addr
            .parse()
            .map_err(|e| format!("Invalid DNS bind address '{bind_addr}': {e}"))?;
        Ok(Self {
            registry,
            bind_addr: addr,
        })
    }

    /// 启动 DNS 服务端，监听 UDP（不返回）。
    ///
    /// # Errors
    /// 当 UDP 绑定或后续 IO 失败时返回错误。
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

    let req_id = request.metadata.id;

    // Only handle standard queries
    if request.metadata.message_type != MessageType::Query
        || request.metadata.op_code != OpCode::Query
    {
        return build_error_response(req_id, ResponseCode::NotImp);
    }

    let mut response = Message::new(req_id, MessageType::Response, OpCode::Query);
    response.metadata.authoritative = true;
    response.metadata.recursion_available = false;

    if request.queries.is_empty() {
        response.metadata.response_code = ResponseCode::FormErr;
        return encode_message(&response);
    }

    let mut response_code = ResponseCode::NoError;

    for question in &request.queries {
        let qname = question.name.to_string();
        let qname = qname.trim_end_matches('.').to_lowercase();

        debug!("DNS query: {} type={:?}", qname, question.query_type);

        match question.query_type {
            RecordType::A => {
                let ips = registry.query_a(&qname).await;
                if ips.is_empty() {
                    response_code = ResponseCode::NXDomain;
                } else {
                    let mut name = question.name.clone();
                    name.set_fqdn(false);
                    for ip in &ips {
                        if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                            let record =
                                Record::from_rdata(name.clone(), 300, RData::A(hickory_proto::rr::rdata::A(addr)));
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
                        if let Ok(target_name) = Name::from_ascii(format!("{target}.")) {
                            let record = Record::from_rdata(
                                question.name.clone(),
                                300,
                                RData::SRV(hickory_proto::rr::rdata::SRV::new(
                                    0,
                                    0,
                                    *port,
                                    target_name,
                                )),
                            );
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

    response.metadata.response_code = response_code;
    encode_message(&response)
}

fn encode_message(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut encoder = BinEncoder::new(&mut buf);
    if msg.emit(&mut encoder).is_err() {
        return build_error_response(msg.metadata.id, ResponseCode::ServFail);
    }
    let bytes = encoder.into_bytes();
    // Truncate to 512 bytes max for UDP DNS
    if bytes.len() > 512 {
        bytes[..512].to_vec()
    } else {
        bytes.clone()
    }
}

fn build_error_response(id: u16, code: ResponseCode) -> Vec<u8> {
    let mut response = Message::new(id, MessageType::Response, OpCode::Query);
    response.metadata.response_code = code;
    encode_message(&response)
}
