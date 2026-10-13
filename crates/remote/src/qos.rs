//! QoS 闭环（阶段 3）：控制器反馈 → 被控端 AIMD。
//!
//! # 数据流
//!
//! ```text
//! 控制器（每秒）──QosFeedback{rtt_ms,recv_kbps,decode_ms,backlog}──▶ 被控端（每秒决策一次）
//!                                                                      │
//!                                          RateController::on_feedback ─┘
//!                                                                      │
//!                        ┌─────────────────────────────────────────────┴───────────────┐
//!                        │ 码率/帧率 → 在线重配置（vendor 补丁 6，不重建、不插 IDR）        │
//!                        │ 分辨率     → 重建编码器 + 强制 IDR（罕见，几何变化）             │
//!                        └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! 反馈是**每秒一条**（控制器侧的心跳 RTT + 解码耗时 + 收码率 + 待解码积压）。被控端本地还
//! 额外量一份**出站队列积压**（编码任务的输出队列），与反馈里的积压一起参与判定。
//!
//! # 为什么是 AIMD 而不是直接设码率
//!
//! 码率/帧率都走**在线重配置**（[`crate::video::VideoEncoder::set_bitrate`] /
//! [`crate::video::VideoEncoder::set_fps`]）。**关键不变量**：普通步进（±10%/20%）**绝不**
//! 重建编码器——重建会强制一个 IDR，AIMD 每秒动一次就变成每秒一个关键帧，正好把阶段 2 要消掉的
//! 带宽尖峰引回来。只有**降编码分辨率**（几何变化，本模块的 `force_keyframe`）才需要重建 + IDR。
//!
//! # 阈值依据（阶段 0 实测，Apple M4 / 10 核）
//!
//! - [`QosParams::delay_threshold_ms`] = 150：交互级延迟红线（对标 RustDesk 的
//!   `DELAY_THRESHOLD_150MS`）。
//! - [`QosParams::fast_threshold_ms`] = 80：明显「链路健康」的延迟（`rtt < 80ms` 才敢升）。
//! - [`QosParams::decode_ms_threshold`] = 37：**控制器侧 4K 解码实测 37–58 ms**——这是
//!   「解码成为瓶颈」的判据下限。分辨率下调的**唯一**依据是它，**不是**编码耗时（1080p 编码
//!   仅 7.9/7.7 ms，根本不是瓶颈）。默认编码宽度 ≤ [`crate::video::MAX_ENCODE_WIDTH`]（1920）
//!   时 1080p 解码远低于 37 ms，故此阈值平时不触发——它是弱控制器 / 显式 4K 路径的安全阀。
//! - [`QosParams::min_bitrate_kbps`] = 200：1080p 静止帧 CBR 下仅 97 B/帧，200 kbps 在 8 fps
//!   下仍有 ~25 KB/帧预算，够日常桌面；再低画面就不可用了，故作为地板。
//! - [`QosParams::min_fps`] = 5：低于 5 fps 交互已无意义（对标 RustDesk 的 `MIN_AUTO_FPS`）。

use crate::screen::{EncodeOptions, EncodeRates};

/// 控制器回报的 QoS 观测量（[`crate::session::SessionMessage::QosFeedback`] 的引擎内表示）。
///
/// 与线上变体字段一一对应，但**不依赖 `SessionMessage`**：本模块是纯状态机，可被单测直接
/// 注入伪造序列（确定性，不依赖真实时钟）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Feedback {
    /// 心跳往返（毫秒）。
    pub rtt_ms: u32,
    /// 控制器接收码率（kbps）。
    pub recv_kbps: u32,
    /// 控制器最近一帧解码耗时（毫秒）。
    pub decode_ms: u32,
    /// 控制器待解码积压（帧数）。
    pub backlog: u32,
}

/// 一次闭环决策的动作分类（供日志与断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosAction {
    /// 保持当前生效值。
    Hold,
    /// 下调（码率 / 帧率 / 分辨率之一）。
    Decrease,
    /// 上调（码率 / 帧率 / 分辨率之一）。
    Increase,
}

/// AIMD 参数。全部有明确实测依据（见模块文档），抽成结构体是为了让单测注入确定性的边界。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QosParams {
    /// 延迟红线（毫秒）：`rtt_ms` 超过它即判拥塞，降 20%。
    pub delay_threshold_ms: u32,
    /// 健康延迟（毫秒）：`rtt_ms` 低于它**且**队列空才升 10%。
    pub fast_threshold_ms: u32,
    /// 解码耗时阈值（毫秒）：帧率已到下限仍拥塞、且解码耗时达到它时，才降编码分辨率。
    pub decode_ms_threshold: u32,
    /// 拥塞时码率的乘性下降因子（0.8 = 降 20%）。
    pub decrease_factor: f32,
    /// 健康时码率的加性上升因子（1.1 = 升 10%）。
    pub increase_factor: f32,
    /// 码率地板（kbps）。
    pub min_bitrate_kbps: u32,
    /// 自动档帧率下限。
    pub min_fps: u8,
    /// 降分辨率时缩放比例的乘性步长（0.75 = 每次降到 3/4）。
    pub scale_step: f32,
    /// 缩放比例地板。
    pub min_scale: f32,
    /// 「持续积压」判定所需的**连续** tick 数（单次抖动不算拥塞）。
    pub backlog_ticks: u32,
}

impl Default for QosParams {
    fn default() -> Self {
        Self {
            delay_threshold_ms: 150,
            fast_threshold_ms: 80,
            decode_ms_threshold: 37,
            decrease_factor: 0.8,
            increase_factor: 1.1,
            min_bitrate_kbps: 200,
            min_fps: 5,
            scale_step: 0.75,
            min_scale: 0.25,
            backlog_ticks: 2,
        }
    }
}

/// 一次决策的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QosDecision {
    /// 决策后的当前生效值（≤ 档位上限）。
    pub rates: EncodeRates,
    /// 是否强制关键帧。**仅在几何变化（降分辨率）时为真**——普通码率/帧率步进绝不置位。
    pub force_keyframe: bool,
    /// 动作分类。
    pub action: QosAction,
}

/// 被控端侧的码率闭环控制器（AIMD）。
///
/// 每次收到控制器的 [`Feedback`] 调用一次 [`RateController::on_feedback`]，产出新的生效值。
/// 档位上限由 [`RateController::set_ceiling`] 设定（`SetQuality` / 会话初始档位）。
#[derive(Debug, Clone)]
pub struct RateController {
    /// AIMD 参数。
    params: QosParams,
    /// 当前档位上限。
    ceiling: EncodeOptions,
    /// 当前生效目标码率（kbps）。
    bitrate_kbps: u32,
    /// 当前生效帧率。
    fps: u8,
    /// 相对档位上限的分辨率降级档数（0 = 用满上限；越大分辨率越低）。
    scale_level: u32,
    /// 分辨率最多能降几档（由 `min_scale` 推出，构造时算好）。
    max_scale_level: u32,
    /// 连续「有积压」的 tick 数。
    backlog_streak: u32,
    /// 上一次动作（日志/断言用）。
    last_action: QosAction,
}

impl RateController {
    /// 以某个档位上限起步（生效值 = 上限）。
    #[must_use]
    pub fn new(ceiling: EncodeOptions) -> Self {
        Self::with_params(ceiling, QosParams::default())
    }

    /// 以指定参数构造（单测注入确定性边界）。
    #[must_use]
    pub fn with_params(ceiling: EncodeOptions, params: QosParams) -> Self {
        let max_scale_level = max_level_for(ceiling.scale, params.scale_step, params.min_scale);
        Self {
            params,
            ceiling,
            bitrate_kbps: ceiling.target_bitrate_kbps,
            fps: ceiling.max_fps,
            scale_level: 0,
            max_scale_level,
            backlog_streak: 0,
            last_action: QosAction::Hold,
        }
    }

    /// 当前档位上限。
    #[must_use]
    pub fn ceiling(&self) -> EncodeOptions {
        self.ceiling
    }

    /// 当前生效值。
    #[must_use]
    pub fn current(&self) -> EncodeRates {
        EncodeRates {
            bitrate_kbps: self.bitrate_kbps,
            fps: self.fps,
            scale: self.effective_scale(),
        }
    }

    /// 上一次动作。
    #[must_use]
    pub fn last_action(&self) -> QosAction {
        self.last_action
    }

    /// 切换档位上限并**重置**闭环（生效值回到新上限）。
    ///
    /// `SetQuality` 走这里：换档位是用户显式意图，闭环的降级历史不应带到新档位。调用方随后
    /// 强制一个关键帧（编码尺寸可能变化）。
    pub fn set_ceiling(&mut self, ceiling: EncodeOptions) {
        self.ceiling = ceiling;
        self.max_scale_level = max_level_for(ceiling.scale, self.params.scale_step, self.params.min_scale);
        self.bitrate_kbps = ceiling.target_bitrate_kbps;
        self.fps = ceiling.max_fps;
        self.scale_level = 0;
        self.backlog_streak = 0;
        self.last_action = QosAction::Hold;
    }

    /// 处理一次反馈，产出新的生效值。
    ///
    /// `local_backlog` 是被控端**本地**出站队列的积压帧数（编码任务的输出队列）；与反馈里的
    /// 控制器侧积压一起参与判定。
    pub fn on_feedback(&mut self, fb: Feedback, local_backlog: u32) -> QosDecision {
        let backed_up = fb.backlog > 0 || local_backlog > 0;
        if backed_up {
            self.backlog_streak = self.backlog_streak.saturating_add(1);
        } else {
            self.backlog_streak = 0;
        }

        let rtt_congested = fb.rtt_ms > self.params.delay_threshold_ms;
        // 拥塞 = 延迟超线 **或** 持续积压（单次积压抖动不算）。
        let congested = rtt_congested || self.backlog_streak >= self.params.backlog_ticks;
        // 健康 = 延迟明显低 **且** 两侧队列都空。
        let healthy = fb.rtt_ms < self.params.fast_threshold_ms && fb.backlog == 0 && local_backlog == 0;

        if congested {
            self.decide_decrease(fb.decode_ms)
        } else if healthy {
            self.decide_increase()
        } else {
            // 中间地带：既不降也不升。
            self.last_action = QosAction::Hold;
            self.decision(false)
        }
    }

    /// 拥塞路径：码率 → 帧率 → 分辨率，逐级用尽。
    fn decide_decrease(&mut self, decode_ms: u32) -> QosDecision {
        let mut force_keyframe = false;
        // 1) 码率降 20%，地板 `min_bitrate_kbps`。
        let target = self.scaled_bitrate(self.params.decrease_factor);
        if target < self.bitrate_kbps {
            self.bitrate_kbps = target;
            self.last_action = QosAction::Decrease;
        } else if self.fps > self.params.min_fps {
            // 2) 码率已在地板：降帧率。
            self.fps -= 1;
            self.last_action = QosAction::Decrease;
        } else if decode_ms >= self.params.decode_ms_threshold && self.scale_level < self.max_scale_level {
            // 3) 帧率也在地板、且**解码**是瓶颈（依据 4K 解码实测 37–58 ms）：降编码分辨率。
            //    这是唯一的「大幅下调」，几何变化 → 重建编码器 + 强制 IDR。
            self.scale_level += 1;
            force_keyframe = true;
            self.last_action = QosAction::Decrease;
        } else {
            self.last_action = QosAction::Hold;
        }
        self.decision(force_keyframe)
    }

    /// 健康路径：码率 → 帧率 → 分辨率，逐级回升。
    ///
    /// 码率回升是加性的（每 tick +10%），从地板回到上限要几十个 tick——这段冷却期天然充当了
    /// 帧率/分辨率恢复的防抖，无需额外的连续健康计数。
    fn decide_increase(&mut self) -> QosDecision {
        let mut force_keyframe = false;
        if self.bitrate_kbps < self.ceiling.target_bitrate_kbps {
            self.bitrate_kbps = self.scaled_bitrate(self.params.increase_factor);
            self.last_action = QosAction::Increase;
        } else if self.fps < self.ceiling.max_fps {
            self.fps += 1;
            self.last_action = QosAction::Increase;
        } else if self.scale_level > 0 {
            self.scale_level -= 1;
            force_keyframe = true;
            self.last_action = QosAction::Increase;
        } else {
            self.last_action = QosAction::Hold;
        }
        self.decision(force_keyframe)
    }

    /// 按因子缩放当前码率并钳到 `[min_bitrate_kbps, ceiling.target_bitrate_kbps]`。
    ///
    /// 用 `round` 而非 `ceil`/`floor`：乘性因子里 `0.8`/`1.1` 都是二进制不可精确表示的小数，
    /// `ceil` 会把 `2560 × 1.1 = 2816.0000000000005` 抬成 2817（虚增 10.04%），四舍五入把这类
    /// 浮点毛刺抹平。
    fn scaled_bitrate(&self, factor: f32) -> u32 {
        let raw = f64::from(self.bitrate_kbps) * f64::from(factor);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "码率为有界正数，取整后落在 u32 内"
        )]
        let scaled = raw.round() as u32;
        scaled
            .max(self.params.min_bitrate_kbps)
            .min(self.ceiling.target_bitrate_kbps)
    }

    /// 当前生效的缩放比例（由 `scale_level` 推出，钳在 `[min_scale, ceiling.scale]`）。
    fn effective_scale(&self) -> f32 {
        let scale = self.ceiling.scale * self.params.scale_step.powi(i32::try_from(self.scale_level).unwrap_or(i32::MAX));
        scale.clamp(self.params.min_scale.min(self.ceiling.scale), self.ceiling.scale)
    }

    /// 组装决策结果。
    fn decision(&self, force_keyframe: bool) -> QosDecision {
        QosDecision {
            rates: self.current(),
            force_keyframe,
            action: self.last_action,
        }
    }
}

/// 求 `ceiling_scale * step^L >= min_scale` 的最大整数 `L`（上限 16，防御性封顶）。
fn max_level_for(ceiling_scale: f32, step: f32, min_scale: f32) -> u32 {
    if step >= 1.0 || ceiling_scale <= min_scale {
        return 0;
    }
    let mut level = 0u32;
    while level < 16 {
        let next = ceiling_scale * step.powi(i32::try_from(level + 1).unwrap_or(0));
        if next < min_scale {
            break;
        }
        level += 1;
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::QualityPreset;

    fn fb(rtt_ms: u32, decode_ms: u32, backlog: u32) -> Feedback {
        Feedback {
            rtt_ms,
            recv_kbps: 0,
            decode_ms,
            backlog,
        }
    }

    fn balanced() -> RateController {
        RateController::new(QualityPreset::Balanced.options())
    }

    #[test]
    fn starts_at_ceiling() {
        let c = balanced();
        assert_eq!(
            c.current(),
            EncodeRates {
                bitrate_kbps: 4000,
                fps: 12,
                scale: 1.0
            }
        );
        assert_eq!(c.ceiling().target_bitrate_kbps, 4000);
    }

    #[test]
    fn congestion_decreases_bitrate_by_20_percent() {
        let mut c = balanced();
        let d = c.on_feedback(fb(200, 10, 0), 0);
        assert_eq!(d.action, QosAction::Decrease);
        assert_eq!(d.rates.bitrate_kbps, 3200, "4000 × 0.8 = 3200");
        assert!(!d.force_keyframe, "普通码率步进绝不插 IDR");
        // 再降一次。
        let d = c.on_feedback(fb(200, 10, 0), 0);
        assert_eq!(d.rates.bitrate_kbps, 2560, "3200 × 0.8 = 2560");
        assert!(!d.force_keyframe);
        // 帧率/分辨率不受影响（还没到地板）。
        assert_eq!(d.rates.fps, 12);
        assert!((d.rates.scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn healthy_increases_bitrate_by_10_percent_capped_at_ceiling() {
        let mut c = balanced();
        // 先降到 2560。
        c.on_feedback(fb(200, 10, 0), 0);
        c.on_feedback(fb(200, 10, 0), 0);
        assert_eq!(c.current().bitrate_kbps, 2560);
        // 健康：升 10%（2560 × 1.1 = 2816）。
        let d = c.on_feedback(fb(50, 10, 0), 0);
        assert_eq!(d.action, QosAction::Increase);
        assert_eq!(d.rates.bitrate_kbps, 2816);
        assert!(!d.force_keyframe);
        // 一直升，绝不超过上限。
        for _ in 0..50 {
            let d = c.on_feedback(fb(50, 10, 0), 0);
            assert!(
                d.rates.bitrate_kbps <= 4000,
                "码率不得越过档位上限：{}",
                d.rates.bitrate_kbps
            );
        }
        assert_eq!(c.current().bitrate_kbps, 4000, "最终应回到上限");
    }

    #[test]
    fn neutral_band_holds() {
        let mut c = balanced();
        // rtt 在 [80, 150] 之间、无积压：既不降也不升。
        let d = c.on_feedback(fb(120, 10, 0), 0);
        assert_eq!(d.action, QosAction::Hold);
        assert_eq!(d.rates.bitrate_kbps, 4000);
    }

    #[test]
    fn single_backlog_tick_is_not_congestion() {
        let mut c = balanced();
        // 单次积压（rtt 正常）：不算拥塞（需连续 2 个 tick）。
        let d = c.on_feedback(fb(50, 10, 1), 0);
        assert_eq!(d.action, QosAction::Hold, "单次积压抖动不该触发降码率");
        // 第二次连续积压 → 判拥塞。
        let d = c.on_feedback(fb(50, 10, 1), 0);
        assert_eq!(d.action, QosAction::Decrease);
        assert_eq!(d.rates.bitrate_kbps, 3200);
    }

    #[test]
    fn local_backlog_counts_toward_congestion() {
        let mut c = balanced();
        // 反馈里的积压为 0，但本地出站队列有积压，连续两 tick → 拥塞。
        c.on_feedback(fb(50, 10, 0), 3);
        let d = c.on_feedback(fb(50, 10, 0), 3);
        assert_eq!(d.action, QosAction::Decrease);
    }

    #[test]
    fn decreases_then_floors_bitrate_then_fps() {
        let mut c = RateController::with_params(
            QualityPreset::Balanced.options(),
            QosParams {
                min_bitrate_kbps: 200,
                min_fps: 5,
                ..QosParams::default()
            },
        );
        // 持续拥塞：先把码率压到地板。
        let mut guard = 0;
        while c.current().bitrate_kbps > 200 && guard < 100 {
            c.on_feedback(fb(300, 10, 0), 0);
            guard += 1;
        }
        assert_eq!(c.current().bitrate_kbps, 200, "码率应压到地板");
        assert_eq!(c.current().fps, 12, "码率未到底前不应动帧率");

        // 继续拥塞：帧率逐 tick 下降。
        for _ in 0..7 {
            c.on_feedback(fb(300, 10, 0), 0);
        }
        assert_eq!(c.current().fps, 5, "帧率应压到下限");
        assert!((c.current().scale - 1.0).abs() < f32::EPSILON, "解码不慢时不降分辨率");
    }

    #[test]
    fn resolution_drops_only_when_decode_is_the_bottleneck() {
        let params = QosParams {
            min_bitrate_kbps: 200,
            min_fps: 5,
            ..QosParams::default()
        };
        // 解码不慢（10ms）：即使码率/帧率都到地板也不降分辨率。
        let mut c = RateController::with_params(QualityPreset::Balanced.options(), params);
        for _ in 0..40 {
            let d = c.on_feedback(fb(300, 10, 0), 0);
            assert!(!d.force_keyframe, "解码不慢时不应降分辨率");
        }
        assert!((c.current().scale - 1.0).abs() < f32::EPSILON);

        // 解码慢（40ms，落进 4K 实测 37–58ms 区间）：码率/帧率到底后降分辨率。
        let mut c = RateController::with_params(QualityPreset::Balanced.options(), params);
        let mut saw_scale_drop = false;
        for _ in 0..40 {
            let d = c.on_feedback(fb(300, 40, 0), 0);
            if d.force_keyframe {
                saw_scale_drop = true;
                break;
            }
        }
        assert!(saw_scale_drop, "解码成为瓶颈时应降分辨率并强制关键帧");
        assert!(c.current().scale < 1.0, "分辨率应下降");
    }

    #[test]
    fn recovery_restores_fps_only_after_bitrate_reaches_ceiling() {
        let params = QosParams {
            min_bitrate_kbps: 200,
            min_fps: 5,
            ..QosParams::default()
        };
        let mut c = RateController::with_params(QualityPreset::Balanced.options(), params);
        // 压到地板 + 帧率下限。
        for _ in 0..40 {
            c.on_feedback(fb(300, 10, 0), 0);
        }
        assert_eq!(c.current().bitrate_kbps, 200);
        assert_eq!(c.current().fps, 5);

        // 健康：码率逐 tick 回升；**在码率回到上限之前，帧率不得恢复**（否则会来回抖）。
        let mut ticks = 0;
        while c.current().bitrate_kbps < 4000 {
            let d = c.on_feedback(fb(50, 10, 0), 0);
            assert_eq!(d.rates.fps, 5, "码率回到上限前不应恢复帧率");
            ticks += 1;
            assert!(ticks < 200, "码率回升不应无限进行");
        }
        // 码率到顶后，帧率开始回升。
        let d = c.on_feedback(fb(50, 10, 0), 0);
        assert_eq!(d.rates.fps, 6, "码率到顶后应恢复帧率");
        assert_eq!(d.action, QosAction::Increase);
    }

    #[test]
    fn bounds_are_never_violated() {
        let mut c = RateController::with_params(
            QualityPreset::Smooth.options(),
            QosParams::default(),
        );
        let seq = [fb(300, 60, 5), fb(10, 0, 0), fb(200, 40, 1), fb(5, 0, 0), fb(150, 0, 0)];
        for _ in 0..200 {
            for f in seq {
                let d = c.on_feedback(f, 0);
                assert!(d.rates.bitrate_kbps >= 200 && d.rates.bitrate_kbps <= 1200);
                assert!(d.rates.fps >= 5 && d.rates.fps <= 8);
                assert!(d.rates.scale >= 0.25 - f32::EPSILON && d.rates.scale <= 0.5 + f32::EPSILON);
            }
        }
    }

    #[test]
    fn set_ceiling_resets_to_new_ceiling() {
        let mut c = balanced();
        c.on_feedback(fb(300, 10, 0), 0);
        assert!(c.current().bitrate_kbps < 4000);
        c.set_ceiling(QualityPreset::Hd.options());
        assert_eq!(
            c.current(),
            EncodeRates {
                bitrate_kbps: 8000,
                fps: 20,
                scale: 1.0
            },
            "换档位应把生效值重置到新上限"
        );
    }

    #[test]
    fn congestion_beats_health_when_both_signals_present() {
        // rtt 极低但队列有积压：拥塞判定优先（不能一边积压一边升码率）。
        let mut c = balanced();
        c.on_feedback(fb(50, 10, 1), 0);
        let d = c.on_feedback(fb(50, 10, 1), 0);
        assert_eq!(d.action, QosAction::Decrease);
    }
}
