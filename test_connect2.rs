
use std::net::{TcpStream, IpAddr, Ipv4Addr, SocketAddr};

fn main() {
    let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 64, 2));
    let addr = SocketAddr::new(ip, 80);
    println!("Connecting to {:?}...", addr);
    match TcpStream::connect(addr) {
        Ok(s) => {
            println!("SUCCESS: Connected! {:?}", s);
        }
        Err(e) => {
            println!("ERROR: Failed to connect: {}", e);
            println!("Error kind: {:?}", e.kind());
            println!("OS error: {}", e.raw_os_error().unwrap_or(-1));
        }
    }
}
