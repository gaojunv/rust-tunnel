//! TLS ClientHello SNI 嗅探：对 `TcpStream` `peek` 首包提取 SNI，不消费字节。
//! 解析失败或非 TLS 一律返回 None（调用方走 HTTP 路径）。

use tokio::net::TcpStream;

/// ClientHello SNI 解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SniParse {
    /// 提取到 SNI（已转小写）
    Sni(String),
    /// 是 ClientHello 但没有 SNI 扩展
    NoSni,
    /// 不是 TLS ClientHello
    NotClientHello,
    /// 数据不完整，需要更多字节
    Incomplete,
}

/// 从 TLS record 字节流解析 ClientHello 的 SNI。
#[must_use] 
pub fn parse_client_hello_sni(buf: &[u8]) -> SniParse {
    // TLS record header: type(1) version(2) length(2)
    if buf.len() < 5 {
        return SniParse::Incomplete;
    }
    if buf[0] != 0x16 {
        // 不是 handshake record
        return SniParse::NotClientHello;
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < 5 + record_len {
        return SniParse::Incomplete;
    }
    let body = &buf[5..5 + record_len];

    // handshake header: type(1) length(3)
    if body.len() < 4 {
        return SniParse::Incomplete;
    }
    if body[0] != 0x01 {
        // 不是 ClientHello
        return SniParse::NotClientHello;
    }
    let mut pos = 4;

    // client_version(2) + random(32)
    if body.len() < pos + 34 {
        return SniParse::Incomplete;
    }
    pos += 34;

    // session_id
    if body.len() < pos + 1 {
        return SniParse::Incomplete;
    }
    let sid_len = body[pos] as usize;
    pos += 1;
    if body.len() < pos + sid_len {
        return SniParse::Incomplete;
    }
    pos += sid_len;

    // cipher_suites
    if body.len() < pos + 2 {
        return SniParse::Incomplete;
    }
    let cs_len = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
    pos += 2;
    if body.len() < pos + cs_len {
        return SniParse::Incomplete;
    }
    pos += cs_len;

    // compression_methods
    if body.len() < pos + 1 {
        return SniParse::Incomplete;
    }
    let cm_len = body[pos] as usize;
    pos += 1;
    if body.len() < pos + cm_len {
        return SniParse::Incomplete;
    }
    pos += cm_len;

    // extensions（可选）
    if body.len() < pos + 2 {
        return SniParse::NoSni; // 无扩展块
    }
    let ext_total = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
    pos += 2;
    if body.len() < pos + ext_total {
        return SniParse::Incomplete;
    }
    let ext_end = pos + ext_total;
    while pos + 4 <= ext_end {
        let ext_type = u16::from_be_bytes([body[pos], body[pos + 1]]);
        let ext_len = u16::from_be_bytes([body[pos + 2], body[pos + 3]]) as usize;
        pos += 4;
        if pos + ext_len > ext_end {
            return SniParse::Incomplete;
        }
        if ext_type == 0x0000 {
            // server_name: list_len(2) name_type(1) name_len(2) name
            let ext = &body[pos..pos + ext_len];
            if ext.len() < 5 {
                return SniParse::NoSni;
            }
            let name_len = u16::from_be_bytes([ext[3], ext[4]]) as usize;
            if ext.len() < 5 + name_len {
                return SniParse::NoSni;
            }
            return match std::str::from_utf8(&ext[5..5 + name_len]) {
                Ok(name) if !name.is_empty() => SniParse::Sni(name.to_ascii_lowercase()),
                _ => SniParse::NoSni,
            };
        }
        pos += ext_len;
    }
    SniParse::NoSni
}

/// Peek 连接首包提取 SNI（带 3 秒总超时，不消费字节）。
///
/// 只在每个新连接读取一次首包；超时、解析失败、非 TLS 一律返回 None，
/// 由调用方继续走正常 HTTP 路径，避免慢连接占用 accept 循环。
pub async fn sniff_sni(stream: &TcpStream) -> Option<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut buf = vec![0u8; 16384];
    loop {
        match tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await {
            Ok(Ok(0)) => return None, // 对端已关闭
            Ok(Ok(n)) => match parse_client_hello_sni(&buf[..n]) {
                SniParse::Sni(name) => return Some(name),
                SniParse::NoSni | SniParse::NotClientHello => return None,
                SniParse::Incomplete => {
                    // 数据未到齐，稍等重试（peek 不消费，重复解析无副作用）
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            },
            Ok(Err(_)) => return None,
            Err(_) => return None, // 超时
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 TLS 1.2 ClientHello record（可选带 SNI 扩展）。
    fn build_client_hello(sni: Option<&str>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // client_version TLS 1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session_id len
        body.extend_from_slice(&[0, 2, 0x13, 0x01]); // cipher_suites len=2
        body.extend_from_slice(&[1, 0]); // compression: len=1, null
        let mut exts = Vec::new();
        if let Some(name) = sni {
            let mut sni_ext = Vec::new();
            let list_len = (1 + 2 + name.len()) as u16;
            sni_ext.extend_from_slice(&list_len.to_be_bytes());
            sni_ext.push(0); // name type: host_name
            sni_ext.extend_from_slice(&(name.len() as u16).to_be_bytes());
            sni_ext.extend_from_slice(name.as_bytes());
            exts.extend_from_slice(&0u16.to_be_bytes()); // ext type: server_name
            exts.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
            exts.extend_from_slice(&sni_ext);
        }
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);

        let mut hs = Vec::new();
        hs.push(0x01); // handshake type: ClientHello
        let len = body.len() as u32;
        hs.extend_from_slice(&len.to_be_bytes()[1..]); // 3-byte length
        hs.extend_from_slice(&body);

        let mut rec = Vec::new();
        rec.push(0x16); // record type: handshake
        rec.extend_from_slice(&[0x03, 0x01]); // record version
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn parses_sni_and_lowercases() {
        let buf = build_client_hello(Some("Trojan.Gaojun.TOP"));
        assert_eq!(
            parse_client_hello_sni(&buf),
            SniParse::Sni("trojan.example.com".to_string())
        );
    }

    #[test]
    fn no_sni_extension() {
        let buf = build_client_hello(None);
        assert_eq!(parse_client_hello_sni(&buf), SniParse::NoSni);
    }

    #[test]
    fn non_tls_bytes() {
        assert_eq!(
            parse_client_hello_sni(b"GET / HTTP/1.1\r\n\r\n"),
            SniParse::NotClientHello
        );
    }

    #[test]
    fn truncated_record_is_incomplete() {
        let buf = build_client_hello(Some("trojan.example.com"));
        assert_eq!(parse_client_hello_sni(&buf[..3]), SniParse::Incomplete);
        let half = buf.len() / 2;
        assert_eq!(parse_client_hello_sni(&buf[..half]), SniParse::Incomplete);
    }

    #[tokio::test]
    async fn sniff_peeks_without_consuming() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hello = build_client_hello(Some("trojan.example.com"));
        let hello_for_client = hello.clone();

        tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            use tokio::io::AsyncWriteExt;
            c.write_all(&hello_for_client).await.unwrap();
            // 保持连接，等服务端 peek + read
            let mut buf = [0u8; 16];
            use tokio::io::AsyncReadExt;
            let _ = c.read(&mut buf).await;
        });

        let (mut server, _) = listener.accept().await.unwrap();
        let sni = sniff_sni(&server).await;
        assert_eq!(sni, Some("trojan.example.com".to_string()));

        // peek 不消费：读出来的字节与原 ClientHello 完全一致
        use tokio::io::AsyncReadExt;
        let mut got = vec![0u8; hello.len()];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(got, hello);
    }
}
