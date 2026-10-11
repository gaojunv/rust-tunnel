//! 数据面加密：X25519 临时密钥交换 + ChaCha20-Poly1305 AEAD。
//!
//! # 为什么需要
//!
//! hbbr 中继**不理解**它转发的内容，屏幕帧在中继路径上是明文可见的（服务端进程也能看到）。
//! 两端都是自研实现，加一层端到端加密成本很低，因此阶段 1 就做掉。
//!
//! # 握手与密钥派生
//!
//! 1. 控制器在 `Hello` 里带一个 X25519 临时公钥，被控端在 `HelloAck` 里带回自己的临时公钥。
//! 2. 双方各做一次 ECDH 得到共享秘密，再用 [`derive_session_keys`] 派生出**两条方向独立的
//!    对称密钥**（`send` / `recv`），从而两个方向的 nonce 计数器互不干扰。
//! 3. 握手帧本身是明文的（此时还没有密钥）；`HelloAck` 之后双方各自装上 [`Cipher`]，
//!    后续所有帧载荷被 AEAD 加密。
//!
//! # 为什么 token 必须进 KDF（**别去掉**）
//!
//! 裸的 ECDH **不足以**抵挡本模块要防的那个对手——中继方。`Hello` / `HelloAck` 都是明文，
//! 中继方可以把两端的临时公钥各自换成自己的，分别与两端完成 ECDH，从而明文读取并**注入**
//! 全部屏幕帧与输入事件。注意 `Hello` 里的 HMAC 覆盖的是
//! `session_id ‖ controller_rd_id ‖ nonce`，**不含**临时公钥；`HelloAck` 则完全没有 MAC。
//!
//! 所以密钥派生的输入必须含一个中继方不知道的秘密——即服务端经**加密控制通道**下发、
//! 且从不上数据面的一次性 `token`。中继方替换公钥后，两端算出的 `shared` 与公钥对都不同，
//! 而它不知道 token，因此既算不出控制器的密钥也算不出被控端的密钥；两端密钥不一致 →
//! 第一帧 AEAD 认证失败 → 会话失败关闭。不替换公钥则它只有密文。它唯一能做的就是丢包，
//! 而那是它本来就有的能力。
//!
//! 派生函数是一个以 SHA-256 为核心的简化 KDF
//! （`token ‖ shared ‖ 域分隔标签 ‖ session_id ‖ 公钥对`）。输入含高熵共享秘密与高熵 token，
//! 且标签把两个方向域分隔开，满足密钥分离需求；这里不引入完整 HKDF 是为了避免额外的
//! 可失败构造调用。

use crate::error::{RemoteError, RemoteResult};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// X25519 公钥长度。
pub const X25519_PUBLIC_LEN: usize = 32;

/// ChaCha20-Poly1305 的 nonce 长度。
const NONCE_LEN: usize = 12;

/// 对称密钥长度。
const KEY_LEN: usize = 32;

/// 一对 X25519 临时密钥。
pub struct EphemeralKeyPair {
    /// 私钥（临时，仅本端持有）。
    secret: StaticSecret,
    /// 对应的公钥。
    public: PublicKey,
}

impl std::fmt::Debug for EphemeralKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 私钥不进日志。
        f.debug_struct("EphemeralKeyPair")
            .field("public", &self.public_bytes())
            .finish_non_exhaustive()
    }
}

impl EphemeralKeyPair {
    /// 生成一对新的临时密钥。
    #[must_use]
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// 本端公钥的字节表示（32 字节）。
    #[must_use]
    pub fn public_bytes(&self) -> [u8; X25519_PUBLIC_LEN] {
        self.public.to_bytes()
    }

    /// 与对端公钥做 ECDH，得到 32 字节共享秘密。
    ///
    /// 拒绝**低阶点**（`was_contributory() == false`）：那类公钥会把共享秘密压成全零之类的
    /// 已知值。混入 token 后攻击者仍拿不到密钥，但这是标准卫生，顺手做掉。
    ///
    /// # Errors
    /// 对端公钥不是 32 字节、或为低阶点时返回 [`RemoteError::Auth`]。
    pub fn diffie_hellman(&self, peer_public: &[u8]) -> RemoteResult<[u8; KEY_LEN]> {
        let arr: [u8; X25519_PUBLIC_LEN] = peer_public
            .try_into()
            .map_err(|_| RemoteError::Auth(format!(
                "对端 X25519 公钥应为 {X25519_PUBLIC_LEN} 字节，实得 {}",
                peer_public.len()
            )))?;
        let shared = self.secret.diffie_hellman(&PublicKey::from(arr));
        if !shared.was_contributory() {
            return Err(RemoteError::Auth("对端 X25519 公钥为低阶点，拒绝".to_owned()));
        }
        Ok(shared.to_bytes())
    }
}

/// 一次会话的两条方向密钥。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKeys {
    /// 本端发送方向使用的密钥。
    pub send: [u8; KEY_LEN],
    /// 本端接收方向使用的密钥。
    pub recv: [u8; KEY_LEN],
}

/// 以 SHA-256 为核心的简化 KDF：
/// `SHA256(标签 ‖ 长度前缀(token) ‖ token ‖ 共享秘密 ‖ 长度前缀(session_id) ‖ session_id ‖ 公钥对)`。
///
/// `token` 是服务端经加密控制通道下发、且**从不上数据面**的一次性秘密。把它作为 KDF 输入
/// 是本次密钥交换的**认证**来源：中继方（攻击者）能替换两端的临时公钥，但它不知道 token，
/// 因此替换后算不出与任一端一致的密钥——两端密钥不一致，第一帧 AEAD 即认证失败，会话
/// 失败关闭。详见模块文档「为什么 token 必须进 KDF」。
fn kdf(
    token: &str,
    shared: &[u8; KEY_LEN],
    session_id: &str,
    initiator_pub: &[u8; 32],
    responder_pub: &[u8; 32],
    label: &[u8],
) -> [u8; KEY_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(b"rt-remote-kdf-v1");
    hasher.update(label);
    hasher.update((token.len() as u64).to_be_bytes());
    hasher.update(token.as_bytes());
    hasher.update(shared);
    hasher.update((session_id.len() as u64).to_be_bytes());
    hasher.update(session_id.as_bytes());
    hasher.update(initiator_pub);
    hasher.update(responder_pub);
    hasher.finalize().into()
}

/// 从共享秘密派生方向密钥。
///
/// `token` 为服务端下发的一次性会话秘密，两端必须传同一个值；它同时承担**认证**职责，
/// 理由见 [`kdf`]。
///
/// `initiator` 表示本端是否为发起方（控制器）。发起方的 `send` 等于被控端的 `recv`，
/// 反之亦然，因此两个方向的 nonce 计数器天然独立、互不干扰。
#[must_use]
pub fn derive_session_keys(
    token: &str,
    shared: &[u8; KEY_LEN],
    session_id: &str,
    initiator_pub: &[u8; 32],
    responder_pub: &[u8; 32],
    initiator: bool,
) -> SessionKeys {
    let k_a = kdf(
        token,
        shared,
        session_id,
        initiator_pub,
        responder_pub,
        b"initiator->responder",
    );
    let k_b = kdf(
        token,
        shared,
        session_id,
        initiator_pub,
        responder_pub,
        b"responder->initiator",
    );
    if initiator {
        SessionKeys { send: k_a, recv: k_b }
    } else {
        SessionKeys { send: k_b, recv: k_a }
    }
}

/// 单向 AEAD 加密器：固定密钥 + 单调递增的 nonce 计数器。
///
/// 每个方向各持一个实例，计数器从 0 开始逐帧递增；TCP 保证帧不丢不乱序，因此两端计数器
/// 始终同步。计数器用 `u64` 回绕（实际会话不可能到达 2^64 帧）。
pub struct Cipher {
    /// AEAD 实例。
    aead: ChaCha20Poly1305,
    /// 下一个 nonce 计数器。
    counter: u64,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 密钥不进日志。
        f.debug_struct("Cipher")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl Cipher {
    /// 用 32 字节对称密钥构造。
    ///
    /// # Errors
    /// 密钥长度不为 32 时返回 [`RemoteError::Auth`]（正常路径下不可能）。
    pub fn new(key: [u8; KEY_LEN]) -> RemoteResult<Self> {
        let aead = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| RemoteError::Auth("ChaCha20-Poly1305 密钥长度非法".to_owned()))?;
        Ok(Self { aead, counter: 0 })
    }

    /// 当前 nonce 计数器（已加密/解密的帧数）。
    #[must_use]
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// 生成下一个 nonce（4 字节零 + 8 字节大端计数器）并推进计数器。
    fn next_nonce(&mut self) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter = self.counter.wrapping_add(1);
        nonce
    }

    /// 加密一段明文，输出「密文 ‖ 16 字节 tag」。
    ///
    /// # Errors
    /// AEAD 加密失败时返回 [`RemoteError::Codec`]。
    pub fn seal(&mut self, plaintext: &[u8]) -> RemoteResult<Vec<u8>> {
        let nonce = self.next_nonce();
        self.aead
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|err| RemoteError::Codec(format!("AEAD 加密失败: {err}")))
    }

    /// 解密一段「密文 ‖ tag」。
    ///
    /// # Errors
    /// 认证失败或密文被篡改时返回 [`RemoteError::Auth`]。
    pub fn open(&mut self, ciphertext: &[u8]) -> RemoteResult<Vec<u8>> {
        let nonce = self.next_nonce();
        self.aead
            .decrypt(Nonce::from_slice(&nonce), ciphertext)
            .map_err(|err| RemoteError::Auth(format!("AEAD 解密/认证失败: {err}")))
    }
}

/// 生成 32 字节随机 nonce（握手防重放用）。
#[must_use]
pub fn random_nonce() -> [u8; KEY_LEN] {
    let mut nonce = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecdh_agrees_on_both_sides() {
        let a = EphemeralKeyPair::generate();
        let b = EphemeralKeyPair::generate();
        let sa = a.diffie_hellman(&b.public_bytes()).unwrap();
        let sb = b.diffie_hellman(&a.public_bytes()).unwrap();
        assert_eq!(sa, sb, "双方 ECDH 应得到同一共享秘密");
        assert_ne!(a.public_bytes(), b.public_bytes(), "两对临时密钥公钥应不同");
    }

    #[test]
    fn bad_peer_key_is_rejected() {
        let a = EphemeralKeyPair::generate();
        assert!(matches!(a.diffie_hellman(&[0u8; 31]), Err(RemoteError::Auth(_))));
    }

    /// 已知的低阶 X25519 公钥（u 坐标）全集。
    ///
    /// 取自 libsodium `crypto_scalarmult_curve25519` 的黑名单，与 RFC 7748 §6.1 提到的
    /// small-order 点一致——`x25519-dalek` 本身**不带**这些测试向量，故在此内置。
    /// 这些点乘以任何「clamp 后」（8 的倍数）的标量都得到单位元，于是共享秘密恒为全零。
    fn low_order_public_keys() -> [[u8; 32]; 7] {
        // p = 2^255 - 19；下面三个是 p-1 / p / p+1 的小端编码（分别对应 -1 / 0 / 1 的非规范编码）。
        let mut p_minus_1 = [0xFFu8; 32];
        p_minus_1[0] = 0xEC;
        p_minus_1[31] = 0x7F;
        let mut p = [0xFFu8; 32];
        p[0] = 0xED;
        p[31] = 0x7F;
        let mut p_plus_1 = [0xFFu8; 32];
        p_plus_1[0] = 0xEE;
        p_plus_1[31] = 0x7F;
        let mut one = [0u8; 32];
        one[0] = 1;
        [
            // 0（阶 4）
            [0u8; 32],
            // 1（阶 1）
            one,
            // 325606250916557431795983626356110631294008115727848805560023387167927233504（阶 8）
            [
                0xE0, 0xEB, 0x7A, 0x7C, 0x3B, 0x41, 0xB8, 0xAE, 0x16, 0x56, 0xE3, 0xFA, 0xF1, 0x9F, 0xC4, 0x6A,
                0xDA, 0x09, 0x8D, 0xEB, 0x9C, 0x32, 0xB1, 0xFD, 0x86, 0x62, 0x05, 0x16, 0x5F, 0x49, 0xB8, 0x00,
            ],
            // 39382357235489614581723060781553021112529911719440698176882885853963445705823（阶 8）
            [
                0x5F, 0x9C, 0x95, 0xBC, 0xA3, 0x50, 0x8C, 0x24, 0xB1, 0xD0, 0xB1, 0x55, 0x9C, 0x83, 0xEF, 0x5B,
                0x04, 0x44, 0x5C, 0xC4, 0x58, 0x1C, 0x8E, 0x86, 0xD8, 0x22, 0x4E, 0xDD, 0xD0, 0x9F, 0x11, 0x57,
            ],
            // p-1（阶 2）
            p_minus_1,
            // p（= 0，阶 4）
            p,
            // p+1（= 1，阶 1）
            p_plus_1,
        ]
    }

    #[test]
    fn low_order_public_key_is_rejected() {
        // 锁的不变量：`diffie_hellman` 必须拒绝**低阶点**（`was_contributory() == false`），
        // 而不只是长度不对的入参。
        //
        // 为什么它此前从未被锁住：`bad_peer_key_is_rejected` 传的是 31 字节，命中的是**长度
        // 分支**，`was_contributory()` 那条路一次都没被执行过。低阶点会把共享秘密压成全零这个
        // **已知值**——攻击者无需知道私钥即可预测派生密钥的一部分，是标准卫生必须挡住的一类输入。
        let a = EphemeralKeyPair::generate();
        for (idx, pk) in low_order_public_keys().iter().enumerate() {
            assert_eq!(pk.len(), X25519_PUBLIC_LEN, "候选点必须是 32 字节");
            assert!(
                matches!(a.diffie_hellman(pk), Err(RemoteError::Auth(_))),
                "第 {idx} 个低阶点必须被拒绝：{pk:?}"
            );
        }
        // 对照组：正常随机公钥应被接受——否则上面的「拒绝」可能只是「一律拒绝」的假象。
        let b = EphemeralKeyPair::generate();
        assert!(a.diffie_hellman(&b.public_bytes()).is_ok(), "正常随机公钥应被接受");
    }

    #[test]
    fn derived_keys_are_directional() {
        let shared = [7u8; 32];
        let ip = [1u8; 32];
        let rp = [2u8; 32];
        let ctrl = derive_session_keys("tok", &shared, "sess-1", &ip, &rp, true);
        let controlled = derive_session_keys("tok", &shared, "sess-1", &ip, &rp, false);
        assert_eq!(ctrl.send, controlled.recv, "发起方 send = 被控方 recv");
        assert_eq!(ctrl.recv, controlled.send, "发起方 recv = 被控方 send");
        assert_ne!(ctrl.send, ctrl.recv, "两个方向密钥应不同");
    }

    #[test]
    fn derived_keys_are_bound_to_session() {
        let shared = [7u8; 32];
        let ip = [1u8; 32];
        let rp = [2u8; 32];
        let a = derive_session_keys("tok", &shared, "sess-1", &ip, &rp, true);
        let b = derive_session_keys("tok", &shared, "sess-2", &ip, &rp, true);
        assert_ne!(a.send, b.send, "不同 session_id 应派生出不同密钥");
    }

    #[test]
    fn derived_keys_are_bound_to_token() {
        // token 是密钥交换的认证来源：不同 token 必须派生出不同密钥，
        // 否则中继方不知道 token 也能算出密钥。
        let shared = [7u8; 32];
        let ip = [1u8; 32];
        let rp = [2u8; 32];
        let a = derive_session_keys("tok-A", &shared, "sess-1", &ip, &rp, true);
        let b = derive_session_keys("tok-B", &shared, "sess-1", &ip, &rp, true);
        assert_ne!(a.send, b.send, "不同 token 应派生出不同密钥");
        assert_ne!(a.recv, b.recv, "不同 token 应派生出不同密钥");
    }

    #[test]
    fn substituted_public_key_yields_mismatched_keys() {
        // MITM 回归防线：中继方把两端的临时公钥各换成自己的，分别与两端 ECDH。
        // 两端用**不同**公钥对派生 → 密钥必然不一致 → 第一帧 AEAD 认证失败、会话失败关闭。
        // 这正是「token 进 KDF」要保证的性质：攻击者不知道 token，无法让两端算出一致的密钥。
        let ctrl_kp = EphemeralKeyPair::generate();
        let ctld_kp = EphemeralKeyPair::generate();
        let mitm_a = EphemeralKeyPair::generate(); // 冒充给被控端的「控制器公钥」
        let mitm_b = EphemeralKeyPair::generate(); // 冒充给控制器的「被控端公钥」

        // 控制器以为对端是 mitm_b；被控端以为对端是 mitm_a。
        let ctrl_shared = ctrl_kp.diffie_hellman(&mitm_b.public_bytes()).unwrap();
        let ctld_shared = ctld_kp.diffie_hellman(&mitm_a.public_bytes()).unwrap();

        let ctrl = derive_session_keys(
            "tok",
            &ctrl_shared,
            "sess-1",
            &ctrl_kp.public_bytes(),
            &mitm_b.public_bytes(),
            true,
        );
        let ctld = derive_session_keys(
            "tok",
            &ctld_shared,
            "sess-1",
            &mitm_a.public_bytes(),
            &ctld_kp.public_bytes(),
            false,
        );
        assert_ne!(
            ctrl.send, ctld.recv,
            "公钥被替换时两端密钥必须不一致（否则中继方可解密）"
        );

        // 对照组：不替换公钥时两端一致。
        let honest_shared_c = ctrl_kp.diffie_hellman(&ctld_kp.public_bytes()).unwrap();
        let honest_shared_s = ctld_kp.diffie_hellman(&ctrl_kp.public_bytes()).unwrap();
        let c2 = derive_session_keys(
            "tok",
            &honest_shared_c,
            "sess-1",
            &ctrl_kp.public_bytes(),
            &ctld_kp.public_bytes(),
            true,
        );
        let s2 = derive_session_keys(
            "tok",
            &honest_shared_s,
            "sess-1",
            &ctrl_kp.public_bytes(),
            &ctld_kp.public_bytes(),
            false,
        );
        assert_eq!(c2.send, s2.recv, "未受攻击时两端密钥应一致");
    }

    #[test]
    fn cipher_roundtrips_and_detects_tampering() {
        let mut sealer = Cipher::new([9u8; 32]).unwrap();
        let mut opener = Cipher::new([9u8; 32]).unwrap();
        let ct = sealer.seal(b"hello screen").unwrap();
        assert_ne!(&ct[..9], b"hello scr", "密文不应等于明文");
        assert_eq!(opener.open(&ct).unwrap(), b"hello screen");

        // 篡改一个字节 → 认证失败。
        let mut bad = ct.clone();
        bad[0] ^= 0xFF;
        assert!(matches!(opener.open(&bad), Err(RemoteError::Auth(_))));
    }

    #[test]
    fn cipher_counters_advance_and_stay_in_sync() {
        let mut sealer = Cipher::new([3u8; 32]).unwrap();
        let mut opener = Cipher::new([3u8; 32]).unwrap();
        for i in 0..8u8 {
            let ct = sealer.seal(&[i]).unwrap();
            assert_eq!(opener.open(&ct).unwrap(), vec![i]);
        }
        assert_eq!(sealer.counter(), 8);
        assert_eq!(opener.counter(), 8);
    }

    #[test]
    fn random_nonce_varies() {
        assert_ne!(random_nonce(), random_nonce(), "随机 nonce 不应重复");
    }
}
