//! `AddrMangle`：控制消息里传递套接字地址所用的编码。
//!
//! 汇合过程中的地址不是明文塞进消息里的，而是经过一层「打乱」——把 IPv4 地址与
//! **当前时间的低 32 位微秒**混进一个 128 位整数，再按小端输出并砍掉末尾的零字节。
//! 它**不是加密**（算法公开，本模块就是实现），目的是让地址不能脱离协议被直接读出来，
//! 同时给同一地址加上时间分量，使它每次传输都不同。
//!
//! # 位布局（IPv4）
//!
//! 设 `stamp` 为微秒时间戳的低 32 位，`ip` 为地址按小端读出的 `u32`，`port` 为端口：
//!
//! ```text
//! value = ((ip + stamp) << 49) | (stamp << 17) | (port + (stamp & 0xFFFF))
//! ```
//!
//! 三段互不重叠：`port + (stamp & 0xFFFF) < 2^17` 占位 0..17，`stamp << 17` 占位
//! 17..49，`ip + stamp < 2^33` 左移 49 后从位 49 起。三段的宽度之和决定了整体不会
//! 溢出到 `u128` 的 128 位之外。
//!
//! 解码是编码的逆运算，但有一个坑：`ip` 被 `+stamp` 掩蔽、`port` 也被 `+stamp` 掩蔽，
//! 所以解出的两个分量都必须先把时间戳减回去。端口那一段还有个细节——低 24 位里同时
//! 混着 `stamp << 17` 的高位，必须先整体减掉时间戳分量、最后再取低 16 位（见
//! [`decode`] 里的注释）。
//!
//! # IPv6
//!
//! IPv6 不做打乱，直接 16 字节地址 + 2 字节小端端口（共 18 字节）。解码时靠长度区分
//! 两种形态：`<= 16` 为 IPv4，`== 18` 为 IPv6。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{SystemTime, UNIX_EPOCH};

/// IPv4 形态的字节上限（`u128` 全宽 16 字节）。
pub const IPV4_MANGLED_MAX_LEN: usize = 16;

/// IPv6 形态的固定字节数：16 字节地址 + 2 字节小端端口。
pub const IPV6_MANGLED_LEN: usize = 18;

/// 低位掩码：`stamp` 与端口分量所在的范围。
const LOW_32_MASK: u128 = u32::MAX as u128;

/// 把 IPv4-mapped 的 IPv6 地址归一成 IPv4（回环地址除外）。
///
/// 双栈监听套接字上收到的 IPv4 连接会表现为 `::ffff:a.b.c.d`，若不归一，同一个
/// 客户端在 v4/v6 两种接入路径下会被当成两个不同来源，打洞与「同内网」判定都会失准。
/// 回环地址（`::1`）保持原样。
///
/// 与上游的差异：上游用 `Ipv6Addr::to_ipv4`，它还会转换已废弃的 IPv4-compatible
/// 形式（`::a.b.c.d`）。这里只处理 IPv4-mapped（`::ffff:a.b.c.d`），因为前者在
/// 现代系统上已不再产生，且转换它反而会把 IPv6 地址误判成 IPv4。
#[must_use]
pub fn try_into_v4(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) if !addr.ip().is_loopback() => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
            None => addr,
        },
        _ => addr,
    }
}

/// 把套接字地址编码成控制消息里传输的字节串。
#[must_use]
pub fn encode(addr: SocketAddr) -> Vec<u8> {
    encode_with_stamp(addr, current_stamp())
}

/// 解码 [`encode`] 产生的字节串。
///
/// 长度为 0..=16 按 IPv4 解、18 按 IPv6 解，其余长度（如 17）判为非法返回 `None`。
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<SocketAddr> {
    if bytes.len() > IPV4_MANGLED_MAX_LEN {
        if bytes.len() != IPV6_MANGLED_LEN {
            return None;
        }
        let octets: [u8; 16] = bytes.get(..16)?.try_into().ok()?;
        let port = u16::from_le_bytes(bytes.get(16..18)?.try_into().ok()?);
        return Some(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(octets),
            port,
            0,
            0,
        )));
    }
    // 编码时砍掉了末尾的零字节，解码要补回来才能还原成 u128。
    let mut padded = [0u8; IPV4_MANGLED_MAX_LEN];
    padded.get_mut(..bytes.len())?.copy_from_slice(bytes);
    let number = u128::from_le_bytes(padded);
    let stamp = (number >> 17) & LOW_32_MASK;
    // 位 49 起是 `ip + stamp`，减掉时间戳分量才是真正的地址。
    let ip = u32::try_from((number >> 49).checked_sub(stamp)?).ok()?;
    // 低 24 位 = `port + (stamp & 0xFFFF)` 与 `(stamp & 0x7F) << 17`。先减掉时间戳
    // 分量，再把高位（属于 `stamp << 17` 的残留）连同端口本身的进位一起截掉。
    let port = ((number & 0x00FF_FFFF).checked_sub(stamp & 0xFFFF)?) & 0xFFFF;
    Some(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(ip.to_le_bytes()),
        u16::try_from(port).ok()?,
    )))
}

/// 取当前时间的低 32 位微秒作为打乱分量。
fn current_stamp() -> u32 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    u32::try_from(micros & LOW_32_MASK).unwrap_or_default()
}

/// [`encode`] 的时间戳可注入版本，供单测对字节布局做定点断言。
fn encode_with_stamp(addr: SocketAddr, stamp: u32) -> Vec<u8> {
    let addr = try_into_v4(addr);
    match addr {
        SocketAddr::V4(v4) => {
            let stamp = u128::from(stamp);
            let ip = u128::from(u32::from_le_bytes(v4.ip().octets()));
            let port = u128::from(v4.port());
            let value = ((ip + stamp) << 49) | (stamp << 17) | (port + (stamp & 0xFFFF));
            let bytes = value.to_le_bytes();
            let keep = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            bytes[..keep].to_vec()
        }
        SocketAddr::V6(v6) => {
            let mut out = v6.ip().octets().to_vec();
            out.extend_from_slice(&v6.port().to_le_bytes());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在给定时间戳下逐字节断言 IPv4 布局。
    ///
    /// 这一条是互操作的主要锚点：任何对位布局的改动都会立刻暴露。
    /// 手算过程：`ip = 127.0.0.1 → 0x0100_007F`，`ip + 1 = 0x0100_0080 = 2^24 + 2^7`，
    /// `port + 1 = 21117 = 0x527D`，故
    /// `value = 2^73 + 2^56 + 2^17 + 0x527D`，小端展开后按字节为
    /// `7D 52 02 00 00 00 00 01 00 02`（末尾 6 个零字节被砍掉）。
    #[test]
    fn ipv4_layout_matches_hand_computed_bytes() {
        let addr = "127.0.0.1:21116".parse().expect("测试地址应可解析");
        assert_eq!(
            encode_with_stamp(addr, 1),
            vec![0x7D, 0x52, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02]
        );
    }

    #[test]
    fn ipv4_roundtrips() {
        let addrs = [
            "0.0.0.0:0",
            "127.0.0.1:1",
            "127.0.0.1:21116",
            "192.168.1.100:60000",
            "10.0.0.1:65535",
            "255.255.255.255:65535",
            "8.8.8.8:53",
            "1.2.3.4:1024",
        ];
        for addr in addrs {
            let addr: SocketAddr = addr.parse().expect("测试地址应可解析");
            let decoded = decode(&encode(addr)).expect("应能解码");
            assert_eq!(decoded, addr, "addr={addr} 往返不一致");
        }
    }

    #[test]
    fn ipv6_roundtrips() {
        for addr in ["[::1]:21116", "[2001:db8::1]:443", "[fe80::1]:65535"] {
            let addr: SocketAddr = addr.parse().expect("测试地址应可解析");
            let encoded = encode(addr);
            assert_eq!(encoded.len(), IPV6_MANGLED_LEN, "addr={addr} IPv6 编码长度");
            assert_eq!(decode(&encoded).expect("应能解码"), addr);
        }
    }

    #[test]
    fn same_address_encodes_differently_over_time_but_decodes_the_same() {
        // 打乱分量的存在意义：同一地址在不同微秒得到不同字节。
        let addr: SocketAddr = "192.168.1.10:21116".parse().unwrap();
        let a = encode_with_stamp(addr, 1);
        let b = encode_with_stamp(addr, 999_999);
        assert_ne!(a, b, "不同时间戳应产生不同编码");
        assert_eq!(decode(&a).unwrap(), addr);
        assert_eq!(decode(&b).unwrap(), addr);
    }

    #[test]
    fn trailing_zero_bytes_are_stripped() {
        let addr: SocketAddr = "127.0.0.1:21116".parse().unwrap();
        let encoded = encode_with_stamp(addr, 1);
        assert!(encoded.len() <= IPV4_MANGLED_MAX_LEN);
        assert_eq!(encoded.last().copied(), Some(0x02), "末字节应为非零");
    }

    #[test]
    fn all_zero_encoding_is_empty_and_decodes_to_unspecified() {
        // 时间戳为 0 且地址端口全 0 时，编码结果为空串——解码必须仍然还原。
        let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        assert!(encode_with_stamp(addr, 0).is_empty());
        assert_eq!(decode(&[]).unwrap(), addr);
    }

    #[test]
    fn invalid_lengths_are_rejected() {
        // 17 字节既不是 IPv4 上限内、也不是 IPv6 的 18 字节。
        assert!(decode(&[0u8; 17]).is_none());
        assert!(decode(&[0u8; 19]).is_none());
        assert!(decode(&[0xFFu8; 64]).is_none());
    }

    #[test]
    fn truncated_garbage_does_not_panic() {
        // 随机短串不应 panic（减法的下溢必须被 checked_sub 拦住）。
        for fill in [0x00u8, 0x01, 0x7F, 0x80, 0xFF] {
            for len in 0..=IPV6_MANGLED_LEN {
                let bytes = vec![fill; len];
                let _ = decode(&bytes);
            }
        }
    }

    #[test]
    fn v4_mapped_ipv6_is_normalized() {
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:21116".parse().unwrap();
        let v4: SocketAddr = "127.0.0.1:21116".parse().unwrap();
        assert_eq!(try_into_v4(mapped), v4);

        // 回环 v6 保持不动。
        let loopback: SocketAddr = "[::1]:21116".parse().unwrap();
        assert_eq!(try_into_v4(loopback), loopback);

        // 真实 v6 地址保持不动。
        let real: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        assert_eq!(try_into_v4(real), real);
    }

    #[test]
    fn mapped_v6_encodes_as_ipv4() {
        let mapped: SocketAddr = "[::ffff:10.1.2.3]:21116".parse().unwrap();
        let v4: SocketAddr = "10.1.2.3:21116".parse().unwrap();
        // 归一后走 IPv4 分支，因此编码长度不会变成 18。
        let encoded = encode_with_stamp(mapped, 12345);
        assert!(encoded.len() <= IPV4_MANGLED_MAX_LEN);
        assert_eq!(decode(&encoded).unwrap(), v4);
    }
}
