//! 硬件编码器的**进程级单槽复用池**（模块名用 `encoder_reuse` 而非 `encoder_pool`，
//! 避免 `clippy::module_name_repetitions` 对 `EncoderPool` 误报）。
//!
//! # 为什么需要复用（真机实测）
//!
//! 无人值守实例每场远程会话都会在编码任务里新建一个 Intel QSV 硬件 MFT 编码器，会话
//! 结束随 `Drop` 销毁。真机实测（release，1080p，复用同一块 Intel D3D11 设备，甚至
//! 一帧都没编码）：每次「建 `MfVideoEncoder` → Drop」都会在驱动侧永久留下约
//! **15 MB working set / 30 MB commit**；H.265 约 26 MB/次，4K H.264 约 33 MB/次。
//! 只做 MFT 枚举不泄漏，只建/销 D3D11 设备也不泄漏——泄漏发生在 MFT 的**实例化/配置**，
//! 驱动在 `Release` 后不归还。同一个编码器跨 20 个「会话」复用则内存完全平
//! （114.8 → 115.8 MB）。NVENC 多轮建/销不累积，因此**只有 MFT 走这个池**（接入点在
//! `win_video.rs` 的 `MftEncoderFactory`）。
//!
//! # 复用规则
//!
//! 键 = 编码标准 + 编码宽高 + 编码器**实际绑定**的采集 D3D11 设备身份（GPU 零拷贝要求
//! 编码器与采集纹理同设备，见 `MftAdapter::encode_frame` 的 `Arc::ptr_eq` 校验）。键不
//! 匹配就销毁池中实例、由调用方新建——几何/设备变化是低频事件，付一次泄漏可以接受。
//! 池里**最多 1 个**实例：远程控制是单会话，不存在并发的第二个编码器。
//!
//! 另有一条与 MFT 有关的约束：MFT 的**峰值码率/VBV 只在创建时设定**（运行期改峰值会
//! 插 IDR，见 `MfVideoEncoder::set_bitrate` 的文档）。所以新会话的目标码率**高于创建时
//! 的目标码率**时不复用——否则关键帧的单帧预算会被旧峰值卡住，整屏文本发虚。目标更低
//! 则安全：峰值只是上限，均值由 `set_bitrate` 在线改。
//!
//! # 复用前必须清空
//!
//! 上一场结束时 MFT 里可能还留着一个已产出、没人取走的 output sample，或停在半截 GOP。
//! 不清空就直接复用，新会话的第一包可能是上一场的 P 帧——对端是**新建的解码器**，解不
//! 出来，要等一个 IDR 才恢复（用户看到的是开场花屏）。清空动作由平台实现提供
//! （[`ReusableEncoder::prepare_reuse`]）：MFT 是 `MFT_MESSAGE_COMMAND_FLUSH` +
//! `MFT_MESSAGE_NOTIFY_START_OF_STREAM`（不重建实例，**不会**触发那 15 MB 泄漏）。
//!
//! # 首帧 IDR 不是这里的责任
//!
//! 引擎在会话首帧就带 `force_keyframe`（`crates/remote/src/controlled.rs` 的
//! `let mut force_keyframe = true`），所以复用后的第一帧必然请求 IDR。MFT 的一帧流水线
//! 延迟（首帧返回空包、下一次调用才吐 IDR）与新建实例时完全一致，复用没有削弱这条保证。
//!
//! # 生命周期
//!
//! 池挂在**工厂实例**上（`HardwareEncoderFactory`，由 `RemoteManagerConfig` 跨会话持有），
//! 不用全局 `static`：全局状态会让单测互相串扰。编码器被引擎取走时返回 [`PooledEncoder`]
//! 包装器：它把所有方法的 `Err` 记成 poisoned（内部状态不可信），`Drop` 时未 poisoned 才
//! 放回池，poisoned 则原地销毁。

use std::sync::{Arc, Mutex, PoisonError};

use rust_tunnel_remote::codec::VideoEncoderApi;
use rust_tunnel_remote::screen::EncoderInfo;
use rust_tunnel_remote::{CodecId, EncodedVideo, RawFrame, RemoteError, RemoteResult};

/// 可以被复用池回收的编码器。
///
/// 在 [`VideoEncoderApi`] 之上只多一条「复用前清空」：不同后端需要的动作不同（MFT 要
/// flush、NVENC 不需要），所以由平台实现决定，而不是由池去猜。
pub trait ReusableEncoder: VideoEncoderApi {
    /// 清掉上一场会话残留的编码状态（pending 输出、半截 GOP、事件/时间戳）。
    ///
    /// 池在**每次取出复用**时调用一次。返回 `Err` 表示该实例状态已不可信，池会直接销毁它
    /// 并让调用方新建，而不是把坏实例交给新会话。
    ///
    /// # Errors
    /// 后端拒绝重置（例如 MFT 不接受 flush 消息）时返回错误。
    fn prepare_reuse(&mut self) -> RemoteResult<()>;
}

/// 复用池的键：只有完全一致才允许复用。
///
/// `device` 是 D3D11 设备的**指针身份**（`Arc::as_ptr` 转成的整数），只用于比较、绝不解
/// 引用：GPU 零拷贝要求编码器与采集纹理在同一块设备上（`MftAdapter::encode_frame` 用
/// `Arc::ptr_eq` 校验），换一块设备就必须换编码器。`None` 表示编码器按 CPU 输入创建，
/// 没有绑定任何采集设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderKey {
    /// 编码标准（H.264 / H.265）。一个工厂只产一种，但放进键里让池可以独立单测。
    pub codec: CodecId,
    /// 编码宽度（像素）。
    pub width: u32,
    /// 编码高度（像素）。
    pub height: u32,
    /// 编码器绑定的采集 D3D11 设备身份（指针值）；`None` = CPU 输入。
    pub device: Option<usize>,
}

/// 池里的一个实例。
struct PoolSlot {
    /// 该实例创建时就位、之后不再变化的键。
    key: EncoderKey,
    /// **创建时**的目标码率（kbps）。MFT 的峰值/VBV 以它为基准，运行期改不了峰值。
    created_bitrate_kbps: u32,
    /// 编码器本体。
    encoder: Box<dyn ReusableEncoder>,
}

/// 进程级单槽编码器复用池。
///
/// 语义是「最多留一个」：远程控制是单会话，没有并发的第二个编码器可用。旧实例被丢弃时
/// 会就地 `Drop`（这正是泄漏的来源，故只在键不匹配/码率上调这种低频路径上发生）。
#[derive(Default)]
pub struct EncoderPool {
    /// 唯一的槽位；`None` = 池空（上一场没有留下实例，或已被取走）。
    slot: Option<PoolSlot>,
}

impl std::fmt::Debug for EncoderPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncoderPool")
            .field("entry", &self.slot.as_ref().map(|slot| slot.key))
            .finish()
    }
}

impl EncoderPool {
    /// 建一个空池。
    #[must_use]
    pub const fn new() -> Self {
        Self { slot: None }
    }

    /// 池里是否没有实例（仅单测使用：生产路径用 `acquire` 的 `None` 判断即可）。
    #[cfg(test)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.slot.is_none()
    }

    /// 取一个键匹配、且能安全承接新会话的实例。
    ///
    /// 复用前依次做三件事，任一步失败都销毁实例并返回 `None`（调用方改用新建）：
    /// 1. 键必须完全一致；
    /// 2. 新目标码率不得高于创建时的目标码率（MFT 的峰值/VBV 只在创建时设定）；
    /// 3. [`ReusableEncoder::prepare_reuse`] 清空上一场残留，再按新会话的
    ///    `target_bitrate_kbps` / `max_fps` 在线重配置。
    ///
    /// 返回的第二个值是该实例**创建时**的码率（峰值基准），调用方应原样带进
    /// [`PooledEncoder`]：后续会话再放回池时仍按这个峰值判断能否复用。
    pub fn acquire(
        &mut self,
        key: EncoderKey,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> Option<(Box<dyn ReusableEncoder>, u32)> {
        let slot = self.slot.take()?;
        if slot.key != key {
            // 几何/设备/标准变了：旧实例与素材对不上，只能销毁（低频路径）。
            return None;
        }
        if target_bitrate_kbps > slot.created_bitrate_kbps {
            // 峰值只在创建时设定，旧实例的峰值低于新会话需要，会让关键帧预算被卡住。
            return None;
        }
        let mut encoder = slot.encoder;
        if let Err(err) = encoder.prepare_reuse() {
            tracing::info!(error = %err, "复用编码器前清空失败，销毁实例改新建");
            return None;
        }
        // 与引擎每帧的在线重配置同口径：只改均值/逻辑帧率，不重建、不插 IDR。
        if let Err(err) = encoder.set_bitrate(target_bitrate_kbps) {
            tracing::info!(error = %err, "复用的编码器不接受新会话码率，销毁实例改新建");
            return None;
        }
        if let Err(err) = encoder.set_fps(max_fps) {
            tracing::info!(error = %err, "复用的编码器不接受新会话帧率，销毁实例改新建");
            return None;
        }
        Some((encoder, slot.created_bitrate_kbps))
    }

    /// 把编码器放回池。池里已有的实例会被替换并就地销毁（单槽语义）。
    pub fn release(
        &mut self,
        key: EncoderKey,
        created_bitrate_kbps: u32,
        encoder: Box<dyn ReusableEncoder>,
    ) {
        self.slot = Some(PoolSlot {
            key,
            created_bitrate_kbps,
            encoder,
        });
    }
}

/// 交给引擎的编码器包装器：委托 + 失败毒化 + 干净的 `Drop` 归还。
///
/// **为什么必须包一层而不是直接把池里的 `Box` 交出去**：引擎用完只会 `Drop`，没有任何
/// 「归还」入口；而只要有一个方法返回过 `Err`，编码器内部状态就不可信（参考链可能已断、
/// MFT 可能卡在异常状态），不能再给下一场会话用。包装器同时解决这两件事。
pub struct PooledEncoder {
    /// 内层编码器；`Drop` 时 `take` 出来放回池。
    inner: Option<Box<dyn ReusableEncoder>>,
    /// 本实例的复用键（归还时原样写回）。
    key: EncoderKey,
    /// 实例**创建时**的目标码率（峰值基准，见 [`EncoderPool::acquire`]）。
    created_bitrate_kbps: u32,
    /// 所属池。
    pool: Arc<Mutex<EncoderPool>>,
    /// 是否已有方法返回过 `Err`。置位后 `Drop` 不归还，直接销毁。
    poisoned: bool,
}

impl PooledEncoder {
    /// 包装一个编码器并挂到池上。
    #[must_use]
    pub fn new(
        inner: Box<dyn ReusableEncoder>,
        key: EncoderKey,
        created_bitrate_kbps: u32,
        pool: Arc<Mutex<EncoderPool>>,
    ) -> Self {
        Self {
            inner: Some(inner),
            key,
            created_bitrate_kbps,
            pool,
            poisoned: false,
        }
    }

    /// 内层编码器（只读）。`Drop` 之后不可能再调用任何方法，故这里的 `None` 不可达。
    fn get(&self) -> &dyn ReusableEncoder {
        self.inner
            .as_deref()
            .unwrap_or_else(|| unreachable!("PooledEncoder 在 Drop 之后不会再被调用"))
    }

    /// 内层编码器（可写）。`Drop` 之后不可能再调用任何方法，故这里的 `None` 分支用错误兜底
    /// （而不是 `unwrap`），既满足「生产代码不 panic」的 lint 门禁，也不会被触发。
    fn inner_mut(&mut self) -> RemoteResult<&mut (dyn ReusableEncoder + 'static)> {
        self.inner
            .as_deref_mut()
            .ok_or_else(|| RemoteError::Encode("编码器已被回收".to_owned()))
    }

    /// 任一方法返回 `Err` 就把自己标记为 poisoned：后续 `Drop` 不再把它放回池。
    ///
    /// 出错后编码器的内部状态不可信（参考链可能已断、MFT 可能停在异常状态），给下一场
    /// 会话复用只会把问题扩散过去。
    fn poison_on_err<T>(&mut self, result: RemoteResult<T>) -> RemoteResult<T> {
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

impl VideoEncoderApi for PooledEncoder {
    fn dimensions(&self) -> (u32, u32) {
        self.get().dimensions()
    }

    fn bitrate_kbps(&self) -> u32 {
        self.get().bitrate_kbps()
    }

    fn fps(&self) -> u32 {
        self.get().fps()
    }

    fn info(&self) -> EncoderInfo {
        self.get().info()
    }

    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        let result = self.inner_mut().and_then(|inner| inner.set_bitrate(kbps));
        self.poison_on_err(result)
    }

    fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        let result = self.inner_mut().and_then(|inner| inner.set_fps(fps));
        self.poison_on_err(result)
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let result = self
            .inner_mut()
            .and_then(|inner| inner.encode(rgba, src_width, src_height, force_keyframe));
        self.poison_on_err(result)
    }

    /// **必须显式委托**：trait 的默认实现会把 `frame.rgba`（GPU 帧时是空）转给 `encode`，
    /// 硬件 MFT 的 GPU 纹理路径（`MftAdapter::encode_frame`）会因此被绕过、整条零拷贝路径
    /// 退化成 CPU 上传。
    fn encode_frame(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let result = self
            .inner_mut()
            .and_then(|inner| inner.encode_frame(frame, force_keyframe));
        self.poison_on_err(result)
    }
}

impl ReusableEncoder for PooledEncoder {
    fn prepare_reuse(&mut self) -> RemoteResult<()> {
        let result = self.inner_mut().and_then(ReusableEncoder::prepare_reuse);
        self.poison_on_err(result)
    }
}

impl Drop for PooledEncoder {
    fn drop(&mut self) {
        // 出过错的内层状态不可信：直接销毁（下一场会话新建），绝不复用。
        if self.poisoned {
            return;
        }
        let Some(inner) = self.inner.take() else {
            return;
        };
        // 锁中毒只可能是上一次 `acquire`/`release` 里 panic；池本身的数据结构仍然完好，
        // 继续用即可（与工厂里其它 `Mutex` 的处理一致）。
        self.pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(self.key, self.created_bitrate_kbps, inner);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    const KEY: EncoderKey = EncoderKey {
        codec: CodecId::H264,
        width: 1920,
        height: 1080,
        device: Some(0x1000),
    };

    /// 假编码器：用原子计数标出「同一实例」与各动作是否真的发生过。
    struct FakeEncoder {
        /// `encode` 调用次数（证明取回的是**同一个实例**，而不是重建的新实例）。
        encodes: Arc<AtomicU32>,
        /// `prepare_reuse` 调用次数（证明取出复用时真的清空了）。
        reuses: Arc<AtomicU32>,
        /// 被销毁的次数（证明池确实没有留下不该留的实例）。
        drops: Arc<AtomicU32>,
        /// `encode` 是否失败（毒化路径）。
        fail_encode: bool,
        /// 当前码率/帧率（委托与在线重配置的断言用）。
        bitrate_kbps: u32,
        fps: u32,
    }

    impl FakeEncoder {
        fn new(
            counters: &(Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>),
            fail_encode: bool,
        ) -> Self {
            Self {
                encodes: Arc::clone(&counters.0),
                reuses: Arc::clone(&counters.1),
                drops: Arc::clone(&counters.2),
                fail_encode,
                bitrate_kbps: 8000,
                fps: 30,
            }
        }
    }

    impl VideoEncoderApi for FakeEncoder {
        fn dimensions(&self) -> (u32, u32) {
            (KEY.width, KEY.height)
        }

        fn bitrate_kbps(&self) -> u32 {
            self.bitrate_kbps
        }

        fn fps(&self) -> u32 {
            self.fps
        }

        fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
            self.bitrate_kbps = kbps;
            Ok(())
        }

        fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
            self.fps = u32::from(fps);
            Ok(())
        }

        fn encode(
            &mut self,
            _rgba: &[u8],
            _src_width: u32,
            _src_height: u32,
            _force_keyframe: bool,
        ) -> RemoteResult<EncodedVideo> {
            self.encodes.fetch_add(1, Ordering::Relaxed);
            if self.fail_encode {
                return Err(RemoteError::Encode("fake 编码失败".to_owned()));
            }
            Ok(EncodedVideo {
                data: vec![0xAA],
                keyframe: false,
            })
        }

        /// 与 `encode` 返回**不同**的数据：包装器若退回 trait 默认实现就会露馅。
        fn encode_frame(
            &mut self,
            _frame: &RawFrame,
            _force_keyframe: bool,
        ) -> RemoteResult<EncodedVideo> {
            Ok(EncodedVideo {
                data: vec![0xEE, 0xEE],
                keyframe: true,
            })
        }

        fn info(&self) -> EncoderInfo {
            EncoderInfo {
                backend: "fake".to_owned(),
                name: "FakeEncoder".to_owned(),
                adapter: "FakeGPU".to_owned(),
                gpu_input: true,
            }
        }
    }

    impl ReusableEncoder for FakeEncoder {
        fn prepare_reuse(&mut self) -> RemoteResult<()> {
            self.reuses.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    impl Drop for FakeEncoder {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 三个计数器（encode / reuse / drop）的便捷构造。
    fn counters() -> (Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>) {
        (
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
        )
    }

    fn frame() -> RawFrame {
        RawFrame {
            display: 0,
            width: 1,
            height: 1,
            // 空 RGBA：包装器若退回 `encode` 默认实现，假编码器会收到它（测试只关心计数）。
            rgba: Vec::new(),
            gpu: None,
            changed: true,
            changed_permille: 1000,
        }
    }

    #[test]
    fn same_key_reuses_instance_and_flushes_first() {
        let counters = counters();
        let mut pool = EncoderPool::new();
        pool.release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));
        assert!(!pool.is_empty());

        let (encoder, created) = pool
            .acquire(KEY, 8000, 30)
            .expect("同键 + 码率不上调应复用");
        assert_eq!(created, 8000, "峰值基准应原样带出");
        assert_eq!(
            counters.1.load(Ordering::Relaxed),
            1,
            "取出时必须清空上一场残留"
        );
        assert_eq!(counters.2.load(Ordering::Relaxed), 0, "取出不应销毁实例");
        assert!(pool.is_empty(), "取出后池必须为空（单槽）");

        // 直接取出的 `Box` 没有包装器帮忙归还；生产路径由 `PooledEncoder::Drop` 调
        // `release`，这里显式调用同样的入口。
        pool.release(KEY, created, encoder);
        assert_eq!(counters.2.load(Ordering::Relaxed), 0, "干净归还不应销毁");
        assert!(!pool.is_empty(), "归还后池里应有实例");
    }

    #[test]
    fn different_key_discards_instance() {
        let counters = counters();
        let mut pool = EncoderPool::new();
        pool.release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));

        let other = EncoderKey { width: 1280, ..KEY };
        assert!(pool.acquire(other, 8000, 30).is_none(), "异键不得复用");
        assert_eq!(counters.2.load(Ordering::Relaxed), 1, "异键应销毁旧实例");
        assert!(pool.is_empty());
    }

    #[test]
    fn higher_bitrate_discards_instance() {
        let counters = counters();
        let mut pool = EncoderPool::new();
        pool.release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));

        assert!(
            pool.acquire(KEY, 12_000, 30).is_none(),
            "目标码率高于创建时的峰值基准时不得复用"
        );
        assert_eq!(counters.2.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reuse_applies_new_session_rates() {
        let counters = counters();
        let mut pool = EncoderPool::new();
        pool.release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));

        let (encoder, _) = pool.acquire(KEY, 3000, 15).expect("降码率应复用");
        assert_eq!(encoder.bitrate_kbps(), 3000, "复用后应带新会话的码率");
        assert_eq!(encoder.fps(), 15, "复用后应带新会话的帧率");
    }

    #[test]
    fn poisoned_encoder_is_not_reused() {
        let counters = counters();
        let pool = Arc::new(Mutex::new(EncoderPool::new()));
        pool.lock()
            .unwrap()
            .release(KEY, 8000, Box::new(FakeEncoder::new(&counters, true)));
        let (inner, created) = pool.lock().unwrap().acquire(KEY, 8000, 30).unwrap();

        let mut wrapped = PooledEncoder::new(inner, KEY, created, Arc::clone(&pool));
        assert!(
            wrapped.encode(&[0; 4], 1, 1, true).is_err(),
            "假编码器应失败"
        );
        drop(wrapped);

        assert!(pool.lock().unwrap().is_empty(), "出错的实例不得放回池");
        assert_eq!(counters.2.load(Ordering::Relaxed), 1, "出错的实例应销毁");
    }

    #[test]
    fn healthy_encoder_returns_to_pool() {
        let counters = counters();
        let pool = Arc::new(Mutex::new(EncoderPool::new()));
        pool.lock()
            .unwrap()
            .release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));
        let (inner, created) = pool.lock().unwrap().acquire(KEY, 8000, 30).unwrap();

        let mut wrapped = PooledEncoder::new(inner, KEY, created, Arc::clone(&pool));
        assert!(wrapped.encode(&[0; 4], 1, 1, true).is_ok());
        assert_eq!(counters.0.load(Ordering::Relaxed), 1);
        drop(wrapped);

        assert!(!pool.lock().unwrap().is_empty(), "正常用完应放回池");
        assert_eq!(counters.2.load(Ordering::Relaxed), 0, "正常用完不应销毁");

        // 再取一次，确认拿到的是同一实例（创建计数继续累加，没有新建）。
        let (mut again, _) = pool.lock().unwrap().acquire(KEY, 3000, 30).unwrap();
        assert!(again.encode(&[0; 4], 1, 1, true).is_ok());
        assert_eq!(counters.0.load(Ordering::Relaxed), 2);
        assert_eq!(counters.2.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn encode_frame_is_delegated_not_fallen_back() {
        let counters = counters();
        let pool = Arc::new(Mutex::new(EncoderPool::new()));
        pool.lock()
            .unwrap()
            .release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));
        let (inner, created) = pool.lock().unwrap().acquire(KEY, 8000, 30).unwrap();
        let mut wrapped = PooledEncoder::new(inner, KEY, created, Arc::clone(&pool));

        let encoded = wrapped.encode_frame(&frame(), true).expect("应委托内层");
        assert_eq!(
            encoded.data,
            vec![0xEE, 0xEE],
            "必须走 encode_frame，而不是 encode"
        );
        assert!(encoded.keyframe);
        assert_eq!(counters.0.load(Ordering::Relaxed), 0, "不得退回 encode");
    }

    #[test]
    fn info_is_delegated() {
        let counters = counters();
        let pool = Arc::new(Mutex::new(EncoderPool::new()));
        pool.lock()
            .unwrap()
            .release(KEY, 8000, Box::new(FakeEncoder::new(&counters, false)));
        let (inner, created) = pool.lock().unwrap().acquire(KEY, 8000, 30).unwrap();
        let wrapped = PooledEncoder::new(inner, KEY, created, Arc::clone(&pool));

        let info = wrapped.info();
        assert_eq!(info.backend, "fake");
        assert_eq!(info.name, "FakeEncoder");
        assert!(info.gpu_input);
    }
}
