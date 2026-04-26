
fn main() {
    let addr = "192.168.64.2:80";
    println!("Connecting to {}...", addr);
    match std::net::TcpStream::connect(addr) {
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
