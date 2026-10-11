//! 会话握手鉴权：一次性 token + HMAC-SHA256。
//!
//! # 威胁模型
//!
//! hbbs **不做身份认证**——任何客户端都能用一个 ID 注册心跳，ID 可被抢占。因此中继/直连
//! 建立后不能只凭「对端自称是谁」就接受会话。服务端在签发会话时，经**加密控制通道**给
//! 双方各下发一个一次性 `token`；控制器在数据面握手时证明自己持有该 token。
//!
//! # 证明方式
//!
//! 控制器发 `HMAC-SHA256(token, session_id ‖ 0x00 ‖ controller_rd_id ‖ 0x00 ‖ nonce)`。
//! 被控端用**挂起 pending 会话里保存的 token** 重算并 `verify_slice`（常数时间比对）。
//! token 从不直接上数据面，因此中继/任何中间人都无法冒充；nonce 由控制器随机生成，
//! 保证同一 token 的每次证明都不同（配合一次性语义防重放）。
//!
//! # 一次性与 TTL
//!
//! 挂起会话 TTL [`HANDSHAKE_TOKEN_TTL`]（60s）。鉴权成功即从表里移除，重复使用会得到
//! [`AuthError::UnknownSession`]（重放被拒）；超时同样被拒。

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rust_tunnel_common::protocol::RemoteMode;

/// 挂起会话的有效期。
pub const HANDSHAKE_TOKEN_TTL: Duration = Duration::from_secs(60);

/// HMAC-SHA256 类型别名。
type HmacSha256 = Hmac<Sha256>;

/// 构造一个 HMAC-SHA256 实例；HMAC 接受任意长度密钥，`None` 表示构造失败（实际不可能）。
fn hmac_new(key: &[u8]) -> Option<HmacSha256> {
    HmacSha256::new_from_slice(key).ok()
}

/// 对一串字段计算 HMAC：每个字段先写 8 字节大端长度再写内容。
///
/// **必须用长度前缀而非固定分隔符**：`nonce` / `x25519_pub` 是二进制，可能含任意字节，
/// 固定分隔符会让 `("a", "bc")` 与 `("ab", "c")` 产生同一输入（拼接歧义）。
///
/// 返回空 `Vec` 表示 HMAC 构造失败（实际不可能）；此时任何校验都会失败，是安全退化。
fn mac_over(token: &str, parts: &[&[u8]]) -> Vec<u8> {
    let Some(mut mac) = hmac_new(token.as_bytes()) else {
        return Vec::new();
    };
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().to_vec()
}

/// 常数时间校验 [`mac_over`] 的产物。
fn verify_over(token: &str, parts: &[&[u8]], mac_bytes: &[u8]) -> bool {
    let Some(mut mac) = hmac_new(token.as_bytes()) else {
        return false;
    };
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    // verify_slice 是常数时间比对，且自带长度校验。
    mac.verify_slice(mac_bytes).is_ok()
}

/// 计算 `Hello` 的握手 MAC。
///
/// 输入：`session_id ‖ controller_rd_id ‖ nonce ‖ x25519_pub`（均长度前缀）。
/// **把 `x25519_pub` 纳入 MAC** 是关键：否则中间人可替换控制器的临时公钥而不被发现。
#[must_use]
pub fn handshake_mac(
    token: &str,
    session_id: &str,
    controller_rd_id: &str,
    nonce: &[u8],
    x25519_pub: &[u8],
) -> Vec<u8> {
    mac_over(
        token,
        &[
            session_id.as_bytes(),
            controller_rd_id.as_bytes(),
            nonce,
            x25519_pub,
        ],
    )
}

/// 常数时间校验 `Hello` 的握手 MAC。
#[must_use]
pub fn verify_handshake_mac(
    token: &str,
    session_id: &str,
    controller_rd_id: &str,
    nonce: &[u8],
    x25519_pub: &[u8],
    mac_bytes: &[u8],
) -> bool {
    verify_over(
        token,
        &[
            session_id.as_bytes(),
            controller_rd_id.as_bytes(),
            nonce,
            x25519_pub,
        ],
        mac_bytes,
    )
}

/// 计算 `HelloAck` 的握手 MAC。
///
/// 输入：`session_id ‖ responder_pub ‖ initiator_pub`。被控端用 token 证明自己就是持有
/// token 的一方，并把**自己的临时公钥**绑定进去；控制器据此拒绝被篡改的 `HelloAck.x25519_pub`。
#[must_use]
pub fn ack_mac(token: &str, session_id: &str, responder_pub: &[u8], initiator_pub: &[u8]) -> Vec<u8> {
    mac_over(token, &[session_id.as_bytes(), responder_pub, initiator_pub])
}

/// 常数时间校验 `HelloAck` 的握手 MAC。
#[must_use]
pub fn verify_ack_mac(
    token: &str,
    session_id: &str,
    responder_pub: &[u8],
    initiator_pub: &[u8],
    mac_bytes: &[u8],
) -> bool {
    verify_over(
        token,
        &[session_id.as_bytes(), responder_pub, initiator_pub],
        mac_bytes,
    )
}

/// 被控端挂起的一个待接入会话。
#[derive(Debug, Clone)]
pub struct PendingSession {
    /// 会话 ID。
    pub session_id: String,
    /// 服务端下发的一次性 token。
    pub token: String,
    /// 发起方客户端名（展示用）。
    pub from_name: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 登记时间，用于 TTL 判定。
    pub created: Instant,
}

impl PendingSession {
    /// 新建一条挂起记录。
    #[must_use]
    pub fn new(session_id: String, token: String, from_name: String, mode: RemoteMode) -> Self {
        Self {
            session_id,
            token,
            from_name,
            mode,
            created: Instant::now(),
        }
    }

    /// 是否已超过 [`HANDSHAKE_TOKEN_TTL`]。
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.created.elapsed() > HANDSHAKE_TOKEN_TTL
    }
}

/// 鉴权失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// 没有该会话的挂起记录（未知 session_id，或已被成功消费 = 重放）。
    UnknownSession,
    /// 挂起记录已超过 TTL。
    Expired,
    /// MAC 校验失败（token 不符或消息被篡改）。
    BadMac,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession => write!(f, "未知或已消费的会话"),
            Self::Expired => write!(f, "挂起会话已过期"),
            Self::BadMac => write!(f, "握手 MAC 校验失败"),
        }
    }
}

/// 被控端挂起会话表（按 `session_id` 索引）。
///
/// 用**同步**互斥锁：所有操作都是极短的临界区、内部无 `await`，而调用方（控制通道的同步
/// 读循环）需要非阻塞地登记/消费，因此不需要异步锁。
#[derive(Debug, Default)]
pub struct PendingSessions {
    /// 内部表。
    inner: Mutex<HashMap<String, PendingSession>>,
}

impl PendingSessions {
    /// 建一张空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条挂起会话（同 ID 覆盖旧记录）。
    ///
    /// 登记时**顺带清扫过期记录**：没有这一步，一条「服务端签发了会话但控制器始终没拨进来」
    /// 的记录会永远留在表里——[`Self::authorize`] 只在被查到且已过期时才顺手移除，没人查就
    /// 没人清。在插入时清扫即可把表大小限制在「最近一个 TTL 窗口内签发的会话」，与服务端
    /// `RemoteSessionTable::issue` 的做法一致。
    pub fn insert(&self, pending: PendingSession) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.retain(|_, existing| !existing.is_expired());
            guard.insert(pending.session_id.clone(), pending);
        }
    }

    /// 取出并移除一条挂起会话（不鉴权）。
    pub fn remove(&self, session_id: &str) -> Option<PendingSession> {
        self.inner.lock().ok().and_then(|mut guard| guard.remove(session_id))
    }

    /// 当前挂起数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |guard| guard.len())
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 鉴权：查找 `session_id` 的挂起记录，校验 TTL 与握手 MAC（含 `x25519_pub`）。
    ///
    /// 成功时**移除**该记录（一次性）；TTL 过期同样移除并返回 [`AuthError::Expired`]。
    /// MAC 失败时保留记录（同一 TTL 窗口内允许对端重试，不影响一次性语义——只有成功
    /// 消费才移除）。
    ///
    /// # Errors
    /// 未知会话、过期或 MAC 不符时返回 [`AuthError`]。
    pub fn authorize(
        &self,
        session_id: &str,
        controller_rd_id: &str,
        nonce: &[u8],
        x25519_pub: &[u8],
        mac_bytes: &[u8],
    ) -> Result<PendingSession, AuthError> {
        let Ok(mut guard) = self.inner.lock() else {
            return Err(AuthError::UnknownSession);
        };
        let Some(pending) = guard.get(session_id).cloned() else {
            return Err(AuthError::UnknownSession);
        };
        if pending.is_expired() {
            guard.remove(session_id);
            return Err(AuthError::Expired);
        }
        if !verify_handshake_mac(
            &pending.token,
            session_id,
            controller_rd_id,
            nonce,
            x25519_pub,
            mac_bytes,
        ) {
            return Err(AuthError::BadMac);
        }
        guard.remove(session_id);
        Ok(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用控制器临时公钥。
    const PUB: &[u8] = &[7u8; 32];

    #[test]
    fn correct_token_verifies() {
        let mac = handshake_mac("tok-123", "sess-1", "123456789", b"nonce-bytes", PUB);
        assert!(verify_handshake_mac(
            "tok-123",
            "sess-1",
            "123456789",
            b"nonce-bytes",
            PUB,
            &mac
        ));
    }

    #[test]
    fn wrong_token_or_fields_fail() {
        let mac = handshake_mac("tok-123", "sess-1", "123456789", b"nonce-bytes", PUB);
        assert!(!verify_handshake_mac("WRONG", "sess-1", "123456789", b"nonce-bytes", PUB, &mac));
        assert!(!verify_handshake_mac("tok-123", "sess-2", "123456789", b"nonce-bytes", PUB, &mac));
        assert!(!verify_handshake_mac("tok-123", "sess-1", "987654321", b"nonce-bytes", PUB, &mac));
        assert!(!verify_handshake_mac("tok-123", "sess-1", "123456789", b"other-nonce", PUB, &mac));
    }

    #[test]
    fn tampered_x25519_pub_breaks_handshake_mac() {
        // 关键：中间人替换控制器的临时公钥必须导致 MAC 校验失败。
        let mac = handshake_mac("tok", "sess-1", "111111111", b"n1", &[7u8; 32]);
        assert!(!verify_handshake_mac(
            "tok",
            "sess-1",
            "111111111",
            b"n1",
            &[9u8; 32],
            &mac
        ));
    }

    #[test]
    fn ack_mac_binds_responder_pubkey() {
        let mac = ack_mac("tok", "sess-1", &[1u8; 32], &[2u8; 32]);
        assert!(verify_ack_mac("tok", "sess-1", &[1u8; 32], &[2u8; 32], &mac));
        // 替换被控端公钥 → 失败。
        assert!(!verify_ack_mac("tok", "sess-1", &[3u8; 32], &[2u8; 32], &mac));
        // 替换 token → 失败。
        assert!(!verify_ack_mac("WRONG", "sess-1", &[1u8; 32], &[2u8; 32], &mac));
    }

    #[test]
    fn length_prefix_prevents_field_splice() {
        // ("ab","c") 与 ("a","bc") 必须产生不同 MAC（长度前缀保证无拼接歧义）。
        let m1 = handshake_mac("t", "ab", "c", b"n", PUB);
        let m2 = handshake_mac("t", "a", "bc", b"n", PUB);
        assert_ne!(m1, m2);
    }

    fn insert(store: &PendingSessions, session_id: &str, token: &str) {
        store.insert(PendingSession::new(
            session_id.to_owned(),
            token.to_owned(),
            "peer-a".to_owned(),
            RemoteMode::Control,
        ));
    }

    #[test]
    fn authorize_accepts_correct_and_rejects_replay() {
        let store = PendingSessions::new();
        insert(&store, "sess-1", "tok");
        let mac = handshake_mac("tok", "sess-1", "111111111", b"n1", PUB);

        assert!(
            store.authorize("sess-1", "111111111", b"n1", PUB, &mac).is_ok(),
            "正确 token 应通过"
        );

        // 重放：记录已被消费。
        let replay = store.authorize("sess-1", "111111111", b"n1", PUB, &mac);
        assert_eq!(replay.unwrap_err(), AuthError::UnknownSession, "重放应被拒");
        assert!(store.is_empty(), "消费后表应为空");
    }

    #[test]
    fn authorize_rejects_bad_mac_and_keeps_entry() {
        let store = PendingSessions::new();
        insert(&store, "sess-1", "tok");
        let bad = handshake_mac("WRONG", "sess-1", "111111111", b"n1", PUB);
        assert_eq!(
            store.authorize("sess-1", "111111111", b"n1", PUB, &bad).unwrap_err(),
            AuthError::BadMac
        );
        // MAC 失败不移除，允许用正确 token 重试。
        let good = handshake_mac("tok", "sess-1", "111111111", b"n1", PUB);
        assert!(store.authorize("sess-1", "111111111", b"n1", PUB, &good).is_ok());
    }

    #[test]
    fn authorize_rejects_expired() {
        let store = PendingSessions::new();
        let mut pending = PendingSession::new(
            "sess-1".to_owned(),
            "tok".to_owned(),
            "peer-a".to_owned(),
            RemoteMode::Control,
        );
        // 手工把登记时间推到 TTL 之前。
        pending.created = Instant::now()
            .checked_sub(HANDSHAKE_TOKEN_TTL + Duration::from_secs(1))
            .expect("测试机运行时间应足够长");
        store.insert(pending);

        let mac = handshake_mac("tok", "sess-1", "111111111", b"n1", PUB);
        assert_eq!(
            store.authorize("sess-1", "111111111", b"n1", PUB, &mac).unwrap_err(),
            AuthError::Expired
        );
        assert!(store.is_empty(), "过期记录应被清理");
    }

    #[test]
    fn insert_sweeps_expired_entries() {
        // 会话签发后控制器始终没拨进来 → 记录不能被无限期留着。
        let store = PendingSessions::new();
        let mut stale = PendingSession::new(
            "sess-old".to_owned(),
            "tok".to_owned(),
            "peer-a".to_owned(),
            RemoteMode::Control,
        );
        stale.created = Instant::now()
            .checked_sub(HANDSHAKE_TOKEN_TTL + Duration::from_secs(1))
            .expect("测试机运行时间应足够长");
        store.insert(stale);
        assert_eq!(store.len(), 1);

        // 新会话登记时，过期的那条被清扫掉。
        insert(&store, "sess-new", "tok2");
        assert_eq!(store.len(), 1, "过期记录应被 insert 清扫");
        assert!(store.remove("sess-old").is_none(), "过期记录不应还在");
        assert!(store.remove("sess-new").is_some(), "新记录应在");
    }

    #[test]
    fn authorize_rejects_unknown_session() {
        let store = PendingSessions::new();
        let mac = handshake_mac("tok", "nope", "111111111", b"n1", PUB);
        assert_eq!(
            store.authorize("nope", "111111111", b"n1", PUB, &mac).unwrap_err(),
            AuthError::UnknownSession
        );
    }
}
