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
//! # 变化面积驱动的生效上限
//!
//! AIMD 只看**链路**（RTT/积压），看不见**内容**：整帧编码 + VBR 会把档位预算与「画面变了
//! 多少」解耦——打字只改几十个像素，编码器照样花完预算。采集层本来就在算像素级变化检测
//! （`win-video` 的 `ChangeReport.changed_permille`），把它喂给
//! [`RateController::observe_activity`]，按三档给**生效值**加一个上限：
//!
//! | 面积 | 等级 | 码率上限 | 帧率上限 |
//! |---|---|---|---|
//! | < 5% | UI | [`QosParams::ui_bitrate_cap_kbps`] 1.2 Mbps | [`QosParams::ui_fps_cap`] 15 |
//! | 5–20% | Mixed | [`QosParams::mixed_bitrate_cap_kbps`] 2.5 Mbps | [`QosParams::mixed_fps_cap`] 30 |
//! | > 20% | Active | 不设上限（AIMD） | 不设上限（AIMD） |
//!
//! 三条设计约束：
//!
//! - **升快降慢**：面积变大立刻放开（视频一开始就要放量），面积变小要连续
//!   [`QosParams::cap_hold_ticks`]（2 秒）才收紧，防止视频里的单个静止帧把上限压死。
//! - **只钳制生效值，不改 AIMD 状态**：`bitrate_kbps`/`fps` 的爬升/下降历史保留，面积一变
//!   大就立刻回到 AIMD 水平，不用重新爬。上限本身也不越过档位上限。
//! - **不置 `force_keyframe`、不改 `scale`**：上限只改在线重配置的码率/帧率，绝不重建编码器
//!   或插 IDR；分辨率降级仍只由「解码耗时成为瓶颈」触发。
//!
//! 面积**不进** `QosFeedback`：那是控制器的网络反馈，面积是被控端本地的采集读数，两者方向
//! 与来源都不同；塞进线上结构要改 `SESSION_PROTO_VERSION`，而本特性不需要跨端信息。
//!
//! # 解码瓶颈与网络拥塞必须分开（阶段 4）
//!
//! 两者的**解法相反**：网络拥塞该降码率/分辨率（少占链路），解码瓶颈再降码率只会让画面更糊
//! 却一点都不减轻解码负担——真正该降的是**帧率**（单位时间要解的帧数），其次分辨率，最后才是
//! 码率。早先的闭环只有一条「码率 → 帧率 → 分辨率」阶梯，解码跟不上时先把码率砍到地板，
//! 用户看到的是「画面又糊又卡」，而解码耗时一点没降。
//!
//! 判据用**解码占用率** `decode_util = decode_ms × fps / 1000`：控制器每秒回报一帧解码耗时，
//! 乘以当前帧率就是「解码器被占用的时间比例」。`>= decode_budget_ratio`（0.8）且（有积压或
//! `>= 1.0`，即单帧已经解不及）即判**饱和**；连续 `decode_bound_ticks`（2）个 tick 才动作，
//! 避免单帧抖动。饱和优先于网络拥塞判定。
//!
//! 网络侧另有一条辅助判据，以及三条**防误判**的硬约束：
//! - **出/收码率比**：被控端知道自己窗口内实际发出去多少（`sent_kbps`），控制器回报收到多少
//!   （`recv_kbps`）。比值 `< delivery_ratio_threshold`（0.7）连续 `delivery_ratio_ticks`（3）
//!   个 tick，说明链路在真实丢包/限速（RTT 可能还没抬升）。`sent_kbps=0` 或 `recv_kbps=0`
//!   是「无样本」哨兵，绝不触发该路径。
//! - **RTT 必须看平滑值 + 连续 tick**：控制器每秒一条心跳样本，单条样本会被事件循环抖动、
//!   中继排队尖峰抬高几十到几百毫秒。直接拿瞬时样本比基线等于把**抖动**当**拥塞**，高 RTT
//!   中继上每秒都在降码率。故判据用 EWMA（[`RTT_EWMA_ALPHA`]）平滑后的 RTT，且要连续
//!   `rtt_congestion_ticks`（2）个 tick 才判拥塞。
//! - **队列深度不能用「> 0」判积压**：解码队列（16 帧）与出站写队列（4 帧）在正常流水线下
//!   **天然非空**——60 fps 下解码器偶尔落后半帧，随机采样到 `backlog > 0` 的概率接近一半。
//!   把「队列里有 1 帧」当拥塞会让闭环**永远**在降码率（表现为会话一开就一路降，几分钟内
//!   逼近地板）。判据因此要求积压达到 `backlog_frames`（4 帧，60 fps 下约
//!   66 ms）或 `local_backlog_frames`（2 帧 = 写队列的一半）才算「持续积压」。
//! - **健康不要求队列精确为空**：恢复只看「没有积压信号」（≤ 上面两个阈值）。要求 `== 0`
//!   会让一条**正常接近容量**运行的链路永远判不健康，码率只降不升，卡在远低于真实容量的
//!   工作点。
//!
//! # 分辨率只由解码瓶颈下调（网络拥塞只动码率/帧率）
//!
//! 阶段 9 曾在网络路径上加过「每像素比特数（bpp）地板」：bpp 低于 0.04 就降分辨率。真机反馈
//! 证明这条规则是错的——它把「链路预算低」直接翻译成「改几何」：1080p 会话几秒内降到
//! 608p、342p，文字完全不可读。而且降分辨率是**几何变化**，要重建编码器 + 强制 IDR，恢复又
//! 排在码率、帧率之后（逐级 +10%），一旦掉下去要一分钟才爬回来。
//!
//! 对远程控制来说，**分辨率是用户第一眼看到的东西**：宁可 1080p 略糊，也不要 342p 的「清晰」。
//! 故网络拥塞只动码率与帧率；分辨率**只**在解码器真的饱和（帧率已到下限、`decode_util` 仍
//! ≥ 1）或网络阶梯到底且 `decode_ms` 越过实测阈值时下调——那是弱控制器上唯一能自救的旋钮，
//! 且对端解码能力不会因为网络变好而变强。需要更低分辨率省带宽的用户应显式选「流畅」档
//! （[`crate::screen::QualityPreset::Smooth`]，0.5 缩放）。
//!
//! # 阈值依据
//!
//! **RTT 判据是相对的**：会话内观测到的最小 RTT（[`RateController::baseline_rtt_ms`]）代表
//! 地理/路径的固定开销，只有 RTT 相对它**显著抬升**（排队时延）才说明链路开始拥塞。中继链路
//! 常年 200 ms 本身不是问题——早先的绝对判据（`rtt > 150ms` 即降）会把它永久判成拥塞，把码率
//! 压到地板、帧率压到下限，画面卡在个位数帧率。
//!
//! **已知局限**：基线只降不升。若会话早期恰好采到一个异常低的样本，此后平滑 RTT 长期高于
//! `基线 + rtt_margin_ms`，`healthy` 就永远不成立，闭环会在码率地板附近停住。暂不做自动重学
//! （抬基线会让**真实**的持续排队时延也变成常态）；真机上把码率压垮的主因是上面那两条误判
//! （队列深度、单样本抖动），先修那两条。日志里的 `rtt_smooth` 与 `baseline` 是判断这个局限
//! 是否被触发的依据。
//!
//! **0 是「尚无样本」的哨兵**（控制器在首个心跳 ack 前用 `last_rtt_ms.unwrap_or(0)` 填 0）：
//! 它不参与基线学习，也不算健康。尚未建立基线时判据退回绝对阈值。
//!
//! - [`QosParams::rtt_margin_ms`] = 120：相对基线的排队时延容限。
//! - [`QosParams::delay_threshold_ms`] = 150：**绝对地板**。基线很低（同内网）时仍不允许
//!   RTT 越过它。实际判据是 `rtt > max(baseline + rtt_margin_ms, delay_threshold_ms)`。
//! - [`QosParams::fast_margin_ms`] = 30：健康 = `rtt <= baseline + 30ms` **且**两侧队列为空。
//! - [`QosParams::decode_ms_threshold`] = 37：**控制器侧 4K 解码实测 37–58 ms**——这是
//!   「解码成为瓶颈」的判据下限。分辨率下调的**唯一**依据是它，**不是**编码耗时（1080p 编码
//!   仅 7.9/7.7 ms，根本不是瓶颈）。默认编码宽度 ≤ [`crate::video::MAX_ENCODE_WIDTH`]（1920）
//!   时 1080p 解码远低于 37 ms，故此阈值平时不触发——它是弱控制器 / 显式 4K 路径的安全阀。
//! - [`QosParams::min_bitrate_kbps`] = 200：1080p 静止帧只需几十字节到几 KB，200 kbps 在
//!   15 fps 下仍有 ~13 KB/帧预算，够日常桌面；再低画面就不可用了，故作为地板。
//! - [`QosParams::min_fps`] = 15：低于 15 fps 交互已明显顿挫。早先照搬 RustDesk 的
//!   `MIN_AUTO_FPS = 5` 是把「链路真差」的地板当成了常态——被控端硬件编码 30+ fps 完全供得起，
//!   地板不该成为默认运行点。

use crate::screen::{EncodeOptions, EncodeRates};

/// 尚无 RTT 基线时的**绝对**健康阈值（毫秒）。
///
/// 只在「还没收到过任何正 RTT 样本」时有意义；正常会话的首个正样本就会建立基线，此后健康判据
/// 一律相对基线。保留它是为了在无基线时仍不把 0（未知哨兵）当成健康。
const ABSOLUTE_FAST_THRESHOLD_MS: u32 = 80;

/// RTT 平滑系数（EWMA）。
///
/// 0.3 ≈ 3 个样本的时间常数。控制器每秒发一条心跳，单条样本会被事件循环抖动、中继排队
/// 尖峰抬高几十到几百毫秒；拿瞬时值比基线等于把**抖动**当**拥塞**。平滑后单条尖峰只看得到
/// 三成幅度，连续两 tick 才越过判据（见 [`QosParams::rtt_congestion_ticks`]）。
const RTT_EWMA_ALPHA: f64 = 0.3;

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
    /// 延迟**绝对地板**（毫秒）：基线很低时也不允许 RTT 越过它。
    pub delay_threshold_ms: u32,
    /// 相对基线的排队时延容限（毫秒）：`rtt > baseline + 本值` 即判拥塞。
    pub rtt_margin_ms: u32,
    /// 健康容限（毫秒）：`rtt <= baseline + 本值` **且**队列空才升 10%。
    pub fast_margin_ms: u32,
    /// 解码耗时阈值（毫秒）：**网络拥塞路径**下、帧率已到下限仍拥塞、且解码耗时达到它时，
    /// 才降编码分辨率。
    pub decode_ms_threshold: u32,
    /// 解码占用率判据（0.8 = 解码器 80% 的时间在忙）：`decode_ms × fps / 1000` 达到它、
    /// 且（有积压或单帧已解不及）即判解码饱和。
    pub decode_budget_ratio: f32,
    /// 连续多少个 tick 判解码饱和才动作（防单帧抖动）。
    pub decode_bound_ticks: u32,
    /// 控制器**解码队列**算「积压」的帧数门槛。
    ///
    /// 队列深度 `> 0` 不是积压：流水线下游总有一帧在途。60 fps 下解码器落后半帧被采样到的
    /// 概率接近一半，用 `> 0` 等于随机判拥塞。4 帧 ≈ 60 fps 下 66 ms 的额外延迟。
    pub backlog_frames: u32,
    /// 被控端**出站写队列**算「积压」的帧数门槛（写队列容量是 4）。
    ///
    /// 2 = 队列过半，说明 writer 真的排不上（中继限速/卡顿），不是瞬时抖动。
    pub local_backlog_frames: u32,
    /// RTT 判据需要的**连续** tick 数（配合 [`RTT_EWMA_ALPHA`] 平滑）。
    pub rtt_congestion_ticks: u32,
    /// 出/收码率比阈值：`recv_kbps / sent_kbps` 低于它即视为链路在丢包/限速。
    pub delivery_ratio_threshold: f32,
    /// 出/收码率比持续低于阈值的 tick 数（达到即判网络拥塞）。
    pub delivery_ratio_ticks: u32,
    /// 拥塞时码率的乘性下降因子（0.8 = 降 20%）。
    pub decrease_factor: f32,
    /// 健康时码率的加性上升因子（1.1 = 升 10%）。
    pub increase_factor: f32,
    /// 码率**远低于上限**（不足上限四分之一）时的乘性上升因子（1.5 = 升 50%）。
    ///
    /// 纯加性 +10% 从地板（200 kbps）回到 10 Mbps 要 41 个 tick（≈41 秒），期间的画面一直
    /// 是糊的。慢启动式快升让恢复落在二十秒内；越过上限的四分之一就切回 +10%，避免在
    /// **真实容量**附近（通常就在上限的 30%–60%）来回过冲——那里用 ×1.5 会形成 2 倍幅度的
    /// 码率锯齿，用户看到的是画质反复变糊变清。
    pub increase_factor_far: f32,
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
    /// **UI 操作**（变化面积很小）时的生效码率上限（kbps）。
    ///
    /// 打字、点菜单、终端光标这类操作每帧只有几个千分点的像素在变，整帧编码 + VBR 却会把
    /// 档位预算花完。1080p 桌面在这种场景下 1.2 Mbps 已足够清晰（按静态文本每帧几 KB、
    /// 15 fps 的量级估算），再高只是给码率控制器送填充。
    pub ui_bitrate_cap_kbps: u32,
    /// **UI 操作**时的生效帧率上限。
    ///
    /// 15 fps 与 [`Self::min_fps`] 同值：UI 操作不需要 30/60 fps 的顺滑度，而降低采集节拍
    /// 本身就能减少「有新帧但内容没变」的无效采集。想更顺滑的用户可选更高画质档位，但上限
    /// 仍受本值约束——这是**内容驱动**的，不是档位驱动的。
    pub ui_fps_cap: u8,
    /// **混合**内容（介于 UI 与运动之间）时的生效码率上限（kbps）。
    pub mixed_bitrate_cap_kbps: u32,
    /// **混合**内容时的生效帧率上限。
    pub mixed_fps_cap: u8,
    /// 变化面积低于此千分比（默认 50 = 5%）视为 **UI 操作**。
    ///
    /// 320×180 网格里 5% ≈ 2880 个格点；打字/菜单的变化远小于它。
    pub ui_ratio_permille: u16,
    /// 变化面积高于此千分比（默认 200 = 20%）视为**运动/视频**（放开到档位上限）。
    pub active_ratio_permille: u16,
    /// **收紧**内容上限需要连续这么多次「更安静」的观测（升档立刻生效，降档迟滞）。
    ///
    /// 默认 2（QoS 每秒一次反馈 ≈ 2 秒）：视频刚开始就要放量，视频停了两秒才收紧。单次
    /// 静止帧（例如视频里的黑场）不该把上限压死。
    pub cap_hold_ticks: u32,
}

impl Default for QosParams {
    fn default() -> Self {
        Self {
            delay_threshold_ms: 150,
            rtt_margin_ms: 120,
            fast_margin_ms: 30,
            decode_ms_threshold: 37,
            decode_budget_ratio: 0.8,
            decode_bound_ticks: 2,
            backlog_frames: 4,
            local_backlog_frames: 2,
            rtt_congestion_ticks: 2,
            delivery_ratio_threshold: 0.7,
            delivery_ratio_ticks: 3,
            decrease_factor: 0.8,
            increase_factor: 1.1,
            increase_factor_far: 1.5,
            min_bitrate_kbps: 200,
            min_fps: 15,
            scale_step: 0.75,
            min_scale: 0.25,
            backlog_ticks: 2,
            ui_bitrate_cap_kbps: 1200,
            ui_fps_cap: 15,
            mixed_bitrate_cap_kbps: 2500,
            mixed_fps_cap: 30,
            ui_ratio_permille: 50,
            active_ratio_permille: 200,
            cap_hold_ticks: 2,
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

/// 由**变化面积**推出的内容活跃等级（`Ord` 用于「升档立刻、降档迟滞」的比较）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Activity {
    /// UI 操作（打字/菜单/光标）：面积 < [`QosParams::ui_ratio_permille`]。
    Ui,
    /// 介于两者之间。
    Mixed,
    /// 运动/视频：面积 > [`QosParams::active_ratio_permille`]。
    Active,
}

impl QosParams {
    /// 某一活跃等级对应的 `(码率上限, 帧率上限)`；`Active` = 不设上限（完全交给 AIMD）。
    fn activity_caps(&self, activity: Activity) -> (Option<u32>, Option<u8>) {
        match activity {
            Activity::Ui => (Some(self.ui_bitrate_cap_kbps), Some(self.ui_fps_cap)),
            Activity::Mixed => (
                Some(self.mixed_bitrate_cap_kbps),
                Some(self.mixed_fps_cap),
            ),
            Activity::Active => (None, None),
        }
    }
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
    /// 连续判「解码饱和」的 tick 数（见模块文档的「解码瓶颈与网络拥塞必须分开」）。
    decode_bound_streak: u32,
    /// 连续「出/收码率比过低」的 tick 数。
    delivery_bad_streak: u32,
    /// 平滑后的 RTT（毫秒，EWMA）；尚无样本时为 `None`。见 [`RTT_EWMA_ALPHA`]。
    rtt_smooth_ms: Option<f64>,
    /// 连续「平滑 RTT 越过拥塞阈值」的 tick 数。
    rtt_congested_streak: u32,
    /// 上一次动作（日志/断言用）。
    last_action: QosAction,
    /// 会话内观测到的**最小** RTT（毫秒），作为地理/路径基线；尚无真实样本时为 `None`。
    ///
    /// 只降不升：它代表链路固定开销，不应随后续排队时延抬升。RTT 为 0 是控制器「尚未采到
    /// 样本」的哨兵（见 `controller.rs` 的 `last_rtt_ms`），不参与基线更新。
    baseline_rtt_ms: Option<u32>,
    /// 当前生效的内容活跃等级（见 [`Self::observe_activity`]）。
    activity: Activity,
    /// 连续「比当前等级更安静」的观测次数（收紧上限的迟滞计数）。
    quiet_streak: u32,
    /// 内容驱动的码率上限（kbps）；`None` = 不压制（`Activity::Active`）。
    ///
    /// **只钳制 [`Self::current`] 的显示/生效值，不改 AIMD 状态**（`bitrate_kbps`）：面积一
    /// 变大就能立刻回到 AIMD 已经爬到/降到的水平，不必重新爬坡。
    content_cap_kbps: Option<u32>,
    /// 内容驱动的帧率上限；语义同 [`Self::content_cap_kbps`]。
    content_cap_fps: Option<u8>,
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
            decode_bound_streak: 0,
            delivery_bad_streak: 0,
            rtt_smooth_ms: None,
            rtt_congested_streak: 0,
            last_action: QosAction::Hold,
            baseline_rtt_ms: None,
            activity: Activity::Active,
            quiet_streak: 0,
            content_cap_kbps: None,
            content_cap_fps: None,
        }
    }

    /// 当前档位上限。
    #[must_use]
    pub fn ceiling(&self) -> EncodeOptions {
        self.ceiling
    }

    /// 当前生效值（AIMD 值再按**内容上限**钳制，且不越过档位上限）。
    #[must_use]
    pub fn current(&self) -> EncodeRates {
        EncodeRates {
            bitrate_kbps: self.effective_bitrate_kbps(),
            fps: self.effective_fps(),
            scale: self.effective_scale(),
        }
    }

    /// 观测一帧的**变化面积**（千分比），据此收紧/放开内容驱动的码率/帧率上限。
    ///
    /// # 为什么面积驱动
    ///
    /// 整帧编码 + VBR 码率控制器的预算与「画面变了多少」无关：打字只改几十个像素，编码器
    /// 照样把档位预算花完（Intel QSV 的 CBR 甚至插填充）。变化面积是**本地免费**的信号
    /// （GPU 变化检测本来就在算），按它分三档给生效值加一个上限，UI 操作就能显著降字节，
    /// 拖窗/滚动/视频仍能放开到档位上限。
    ///
    /// # 升快降慢
    ///
    /// **升档（要求更高码率）立刻生效**：视频/拖窗一开始就要放量，晚一秒用户就看到糊。
    /// **降档（收紧上限）要连续 [`QosParams::cap_hold_ticks`] 次**「更安静」的观测：视频里的
    /// 单个静止帧（黑场、暂停画面）不该把上限压死；视频真正停了约两秒后才收紧。
    ///
    /// # 为什么不进线上结构
    ///
    /// `changed_permille` 是被控端**本地**采集层的读数，`QosFeedback` 是控制器的网络反馈，
    /// 两者方向与来源都不同。把它塞进 `QosFeedback` 会改动线上结构（`SESSION_PROTO_VERSION`
    /// 与所有旧客户端），而本特性完全不需要跨端信息。
    ///
    /// **不置关键帧、不重建编码器**：上限只改 [`Self::current`] 的码率/帧率生效值，走既有的
    /// 在线重配置（`set_bitrate` / `set_fps`），`scale` 与 `force_keyframe` 一概不动。
    pub fn observe_activity(&mut self, changed_permille: u16) {
        let observed = if changed_permille < self.params.ui_ratio_permille {
            Activity::Ui
        } else if changed_permille > self.params.active_ratio_permille {
            Activity::Active
        } else {
            Activity::Mixed
        };

        if observed >= self.activity {
            // 升档或持平：立刻生效（视频刚开始就要放量）。
            self.activity = observed;
            self.quiet_streak = 0;
        } else {
            // 降档（收紧上限）：连续安静够久才生效。
            self.quiet_streak = self.quiet_streak.saturating_add(1);
            if self.quiet_streak >= self.params.cap_hold_ticks.max(1) {
                self.activity = observed;
                self.quiet_streak = 0;
            }
        }

        let (cap_kbps, cap_fps) = self.params.activity_caps(self.activity);
        self.content_cap_kbps = cap_kbps;
        self.content_cap_fps = cap_fps;
    }

    /// 当前**内容驱动**的码率上限（kbps）；`None` = 不压制。仅用于日志/断言。
    #[must_use]
    pub fn content_cap_kbps(&self) -> Option<u32> {
        self.content_cap_kbps
    }

    /// 当前**内容驱动**的帧率上限；`None` = 不压制。仅用于日志/断言。
    #[must_use]
    pub fn content_cap_fps(&self) -> Option<u8> {
        self.content_cap_fps
    }

    /// 上一次动作。
    #[must_use]
    pub fn last_action(&self) -> QosAction {
        self.last_action
    }

    /// 会话内观测到的最小 RTT（地理/路径基线）；尚无真实样本时为 `None`。
    #[must_use]
    pub fn baseline_rtt_ms(&self) -> Option<u32> {
        self.baseline_rtt_ms
    }

    /// EWMA 平滑后的 RTT（毫秒）；尚无真实样本时为 `None`。仅用于日志/断言。
    #[must_use]
    pub fn smoothed_rtt_ms(&self) -> Option<f64> {
        self.rtt_smooth_ms
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
        self.decode_bound_streak = 0;
        self.delivery_bad_streak = 0;
        self.rtt_congested_streak = 0;
        self.last_action = QosAction::Hold;
    }

    /// 只改帧率**上限**（控制器显式选择 30 / 60），码率 / 缩放 / 闭环历史一概不动。
    ///
    /// 与 [`Self::set_ceiling`] 的区别：换画质档位是「换一整套上限」，而帧率选择只覆盖
    /// `max_fps`——用户选了 60 之后再把画质切到「流畅」，帧率仍应是 60（调用方在 `SetQuality`
    /// 之后重新应用本方法，见 `controlled.rs`）。
    ///
    /// `fps` 被钳到 `[min_fps, 120]`；当前生效帧率同步到新上限（用户显式意图，重置到上限），
    /// 下一次 QoS 反馈再按网络状况在它之下调整。**不改编码器**：调用方走在线重配置。
    pub fn set_frame_rate_ceiling(&mut self, fps: u8) {
        let fps = fps.clamp(self.params.min_fps.max(1), 120);
        self.ceiling.max_fps = fps;
        self.fps = fps;
    }

    /// 处理一次反馈，产出新的生效值。
    ///
    /// `local_backlog` 是被控端**本地**出站队列的积压帧数（编码任务的输出队列）；与反馈里的
    /// 控制器侧积压一起参与判定。`sent_kbps` 是被控端本窗口实际发出的视频码率（编码任务的出码
    /// 字节速率），与控制器回报的 `recv_kbps` 一起算**投递率**——RTT 还没抬升时，投递率是唯一
    /// 能看出链路在丢包/限速的信号。
    pub fn on_feedback(&mut self, fb: Feedback, local_backlog: u32, sent_kbps: u32) -> QosDecision {
        // 会话内最小 RTT = 地理/路径基线（只降不升）。RTT 为 0 是控制器「尚无样本」的哨兵，
        // 不能让它把基线永久钉在 0（否则真实链路再也判不成健康、也永远相对基线上涨）。
        if fb.rtt_ms > 0 {
            self.baseline_rtt_ms = Some(self.baseline_rtt_ms.map_or(fb.rtt_ms, |old| old.min(fb.rtt_ms)));
            // 平滑 RTT：单条样本的抖动不该被当成排队时延（见 [`RTT_EWMA_ALPHA`]）。
            let sample = f64::from(fb.rtt_ms);
            self.rtt_smooth_ms = Some(match self.rtt_smooth_ms {
                Some(prev) => prev * (1.0 - RTT_EWMA_ALPHA) + sample * RTT_EWMA_ALPHA,
                None => sample,
            });
        }

        // 积压必须达到**队列门槛**才算：队列非空是流水线的常态，不是拥塞（见模块文档）。
        let decode_backlog = fb.backlog >= self.params.backlog_frames;
        let backed_up = decode_backlog || local_backlog >= self.params.local_backlog_frames;
        if backed_up {
            self.backlog_streak = self.backlog_streak.saturating_add(1);
        } else {
            self.backlog_streak = 0;
        }

        // 解码占用率：`decode_ms × fps / 1000` = 解码器忙于解码的时间比例。
        // `decode_ms == 0` 是「尚无解码样本」哨兵（首帧前），不参与饱和判定。
        let fps = self.current().fps.max(1);
        let decode_util = f64::from(fb.decode_ms) * f64::from(fps) / 1000.0;
        let saturated = fb.decode_ms > 0
            && decode_util >= f64::from(self.params.decode_budget_ratio)
            && (decode_backlog || decode_util >= 1.0);
        if saturated {
            self.decode_bound_streak = self.decode_bound_streak.saturating_add(1);
        } else {
            self.decode_bound_streak = 0;
        }

        // 投递率：`recv/sent` 持续低于阈值 = 链路在丢/限。任一侧为 0 是「无样本」哨兵，
        // 绝不触发（否则会话刚开始或静止画面会把闭环误判成丢包）。
        let delivery_bad = sent_kbps >= 100
            && fb.recv_kbps > 0
            && f64::from(fb.recv_kbps) / f64::from(sent_kbps) < f64::from(self.params.delivery_ratio_threshold);
        if delivery_bad {
            self.delivery_bad_streak = self.delivery_bad_streak.saturating_add(1);
        } else {
            self.delivery_bad_streak = 0;
        }
        let delivery_bad_sustained = self.delivery_bad_streak >= self.params.delivery_ratio_ticks;

        // 拥塞 = 平滑 RTT 相对基线显著抬升 **或** 持续积压 **或** 投递率持续过低。
        // 尚无基线时退回绝对地板。绝对地板保证基线很低时仍不允许高延迟。
        let congestion_threshold = self.baseline_rtt_ms.map_or(self.params.delay_threshold_ms, |baseline| {
            baseline
                .saturating_add(self.params.rtt_margin_ms)
                .max(self.params.delay_threshold_ms)
        });
        // 用**平滑值**比阈值，并要求连续 `rtt_congestion_ticks` 个 tick：单条心跳样本的抖动
        // （事件循环调度、中继排队尖峰）不该触发降码率。
        if self.rtt_smooth_ms.is_some_and(|rtt| rtt > f64::from(congestion_threshold)) {
            self.rtt_congested_streak = self.rtt_congested_streak.saturating_add(1);
        } else {
            self.rtt_congested_streak = 0;
        }
        let rtt_congested = self.rtt_congested_streak >= self.params.rtt_congestion_ticks;
        let congested = rtt_congested || self.backlog_streak >= self.params.backlog_ticks || delivery_bad_sustained;
        // 健康 = RTT 贴着基线（无基线时按绝对阈值）**且**两侧队列都没到积压门槛，**且**解码
        // 未饱和、投递率未恶化。
        // `rtt_ms == 0` 是未知哨兵：既不算拥塞，也绝不能算健康（否则会一边「无样本」一边升码率）。
        // 注意用 `healthy_rtt`（瞬时样本）而不是 `rtt_congested` 取反：中间地带（平滑值已过
        // 门槛但还没攒够 tick）不升也不降。
        let healthy_rtt = fb.rtt_ms > 0
            && !self.rtt_smooth_ms.is_some_and(|rtt| rtt > f64::from(congestion_threshold))
            && self.baseline_rtt_ms.map_or(fb.rtt_ms < ABSOLUTE_FAST_THRESHOLD_MS, |baseline| {
                fb.rtt_ms <= baseline.saturating_add(self.params.fast_margin_ms)
            });
        let healthy = healthy_rtt && !backed_up && !saturated && !delivery_bad;

        // 顺序：解码饱和优先。它说明瓶颈在控制器解码器，降码率只会让画面更糊而不减轻解码负担。
        if self.decode_bound_streak >= self.params.decode_bound_ticks {
            self.decide_decrease_decode()
        } else if congested {
            self.decide_decrease_network(fb.decode_ms)
        } else if healthy {
            self.decide_increase()
        } else {
            // 中间地带：既不降也不升。
            self.last_action = QosAction::Hold;
            self.decision(false)
        }
    }

    /// 解码瓶颈路径：**先降帧率**（直接减轻单位时间的解码量），再降分辨率，最后才砍码率。
    ///
    /// 与网络拥塞路径相反：这里砍码率对解码耗时几乎没有帮助（解码成本主要由像素数与帧数决定），
    /// 只会让画面先糊掉。码率只作为最后的地板兜底。
    fn decide_decrease_decode(&mut self) -> QosDecision {
        let mut force_keyframe = false;
        if self.fps > self.params.min_fps {
            // 1) 降帧率：每秒少解几帧，解码占用率立刻下降。
            //
            // **乘性下降（×3/4）而不是 -1**：解码饱和是「现在就解不动」，从 60 降到 15
            // 如果每 tick 只减 1，要 45 秒才收敛，期间画面一直在积压/丢帧。乘性下降
            // 5 个 tick 就能从 60 落到 15，与网络路径的降级节奏接近。
            let target = (u32::from(self.fps) * 3 / 4).max(u32::from(self.params.min_fps));
            #[expect(
                clippy::cast_possible_truncation,
                reason = "目标帧率被钳在 [min_fps, 当前 fps] 之间，落在 u8 内"
            )]
            {
                self.fps = target as u8;
            }
            self.last_action = QosAction::Decrease;
        } else if self.scale_level < self.max_scale_level {
            // 2) 帧率已到下限：降编码分辨率（几何变化 → 重建 + 强制 IDR）。
            self.scale_level += 1;
            force_keyframe = true;
            self.last_action = QosAction::Decrease;
        } else {
            // 3) 分辨率也用尽：码率再降 10%（地板）——聊胜于无，但不越过最小码率。
            let target = self.scaled_bitrate(0.9);
            if target < self.bitrate_kbps {
                self.bitrate_kbps = target;
                self.last_action = QosAction::Decrease;
            } else {
                self.last_action = QosAction::Hold;
            }
        }
        self.decision(force_keyframe)
    }

    /// 网络拥塞路径：码率 → 帧率 → 分辨率，**每一级都有门槛**。
    ///
    /// 网络拥塞**绝不**因为「码率相对像素显得不够」就改几何：分辨率是用户第一眼的观感，
    /// 而且改几何要重建编码器 + 强制 IDR、恢复极慢（见模块文档「分辨率只由解码瓶颈下调」）。
    /// 只有在码率、帧率都到地板**且**对端解码耗时确实越过实测阈值时，才用分辨率当最后的兜底。
    fn decide_decrease_network(&mut self, decode_ms: u32) -> QosDecision {
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
    /// 码率**远低于上限**（不足上限四分之一）时乘性快升（[`QosParams::increase_factor_far`]），
    /// 越过四分之一后切回加性 +10%。纯加性从地板（200 kbps）爬回 10 Mbps 要 41 秒，而分辨率
    /// 恢复又排在码率、帧率之后——用户看到的就是「掉下去一分钟才回来」。快升阶段的过冲由
    /// 拥塞判据的连续 tick 确认兜住。
    fn decide_increase(&mut self) -> QosDecision {
        let mut force_keyframe = false;
        if self.bitrate_kbps < self.ceiling.target_bitrate_kbps {
            let factor = if self.bitrate_kbps.saturating_mul(4) < self.ceiling.target_bitrate_kbps {
                self.params.increase_factor_far
            } else {
                self.params.increase_factor
            };
            self.bitrate_kbps = self.scaled_bitrate(factor);
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

    /// AIMD 码率再按内容上限钳制；上限本身不越过档位上限，也不低于地板。
    ///
    /// 只影响返回值：`self.bitrate_kbps` 是 AIMD 的长期状态，面积变大时直接恢复。
    fn effective_bitrate_kbps(&self) -> u32 {
        let ceiling_kbps = self
            .ceiling
            .target_bitrate_kbps
            .max(self.params.min_bitrate_kbps);
        let cap = self
            .content_cap_kbps
            .unwrap_or(ceiling_kbps)
            .clamp(self.params.min_bitrate_kbps, ceiling_kbps);
        self.bitrate_kbps.min(cap)
    }

    /// AIMD 帧率再按内容上限钳制，并夹到 `[min_fps, ceiling.max_fps]`。
    fn effective_fps(&self) -> u8 {
        let min_fps = self.params.min_fps.max(1);
        let ceiling_fps = self.ceiling.max_fps.max(min_fps);
        let cap = self
            .content_cap_fps
            .unwrap_or(ceiling_fps)
            .clamp(min_fps, ceiling_fps);
        self.fps.min(cap).clamp(min_fps, ceiling_fps)
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

    /// 先喂一个低 RTT 样本建立基线。**必须**先建基线：否则首个高 RTT 样本会把基线设成它自己，
    /// 相对判据会认为「一直就这么慢」，不判拥塞。
    fn prime(c: &mut RateController) {
        c.on_feedback(fb(50, 10, 0), 0, 0);
    }

    /// 反复喂同一类反馈直到出现 `want` 动作（最多 10 tick），返回用掉的 tick 数。
    ///
    /// RTT 判据现在要**平滑值**越过阈值且连续 `rtt_congestion_ticks` 个 tick，所以单条高 RTT
    /// 样本不再立刻降级——测试必须按真实节拍连喂几条。
    fn feed_until(c: &mut RateController, f: Feedback, want: QosAction) -> u32 {
        for tick in 1..=10 {
            if c.on_feedback(f, 0, 0).action == want {
                return tick;
            }
        }
        panic!("连续 10 个 tick 内未出现 {want:?}");
    }

    #[test]
    fn starts_at_ceiling() {
        let c = balanced();
        assert_eq!(
            c.current(),
            EncodeRates {
                bitrate_kbps: 6000,
                fps: 30,
                scale: 1.0
            }
        );
        assert_eq!(c.ceiling().target_bitrate_kbps, 6000);
        assert_eq!(c.baseline_rtt_ms(), None, "首个反馈前没有基线");
    }

    #[test]
    fn congestion_decreases_bitrate_by_20_percent() {
        let mut c = balanced();
        prime(&mut c); // 基线 50，平滑值 50
        // 单条 300ms 样本只把平滑值抬到 50×0.7 + 300×0.3 = 125（阈值 170），不该立刻降；
        // 第二条到 177.5（越过阈值，但还没攒够 2 个 tick）；第三条才判拥塞。
        let d = c.on_feedback(fb(300, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Hold, "单条 RTT 尖峰是噪声，不是拥塞");
        let d = c.on_feedback(fb(300, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Hold, "越过阈值还需连续 2 个 tick");
        let d = c.on_feedback(fb(300, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Decrease);
        assert_eq!(d.rates.bitrate_kbps, 4800, "6000 × 0.8 = 4800");
        assert!(!d.force_keyframe, "普通码率步进绝不插 IDR");
        // 持续拥塞：继续降。
        let d = c.on_feedback(fb(300, 10, 0), 0, 0);
        assert_eq!(d.rates.bitrate_kbps, 3840, "4800 × 0.8 = 3840");
        assert!(!d.force_keyframe);
        // 帧率/分辨率不受影响（还没到地板）。
        assert_eq!(d.rates.fps, 30);
        assert!((d.rates.scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn healthy_increases_bitrate_by_10_percent_capped_at_ceiling() {
        let mut c = balanced();
        prime(&mut c);
        // 先降到 3840（两次乘性下降）。
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        assert_eq!(c.current().bitrate_kbps, 3840);
        // RTT 回到基线：平滑值要 1–2 个 tick 才衰减回阈值以下（余温最多再降一档），
        // 随后转入回升。
        let ticks = feed_until(&mut c, fb(50, 10, 0), QosAction::Increase);
        assert!(ticks <= 3, "RTT 回落后应在 3 个 tick 内转为回升，实际 {ticks}");
        assert!(
            c.current().bitrate_kbps >= 3000,
            "余温最多再降一档，实际 {}",
            c.current().bitrate_kbps
        );
        // 一直升，绝不超过上限。
        for _ in 0..50 {
            let d = c.on_feedback(fb(50, 10, 0), 0, 0);
            assert!(
                d.rates.bitrate_kbps <= 6000,
                "码率不得越过档位上限：{}",
                d.rates.bitrate_kbps
            );
        }
        assert_eq!(c.current().bitrate_kbps, 6000, "最终应回到上限");
    }

    #[test]
    fn neutral_band_holds() {
        let mut c = balanced();
        prime(&mut c);
        // 基线 50：rtt 在 (50+30, 50+120] 之间、无积压——既不降也不升。
        let d = c.on_feedback(fb(120, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Hold);
        assert_eq!(d.rates.bitrate_kbps, 6000);
    }

    /// **正常流水线的队列深度不是拥塞**：60 fps 下解码器偶尔落后半帧，采样到 `backlog > 0`
    /// 的概率接近一半。旧判据用 `> 0`，等于随机每秒降一次码率——这是「会话一开就一路降、
    /// 带宽低到画面不可用」的根因。
    #[test]
    fn shallow_pipeline_queue_is_not_congestion() {
        let mut c = balanced();
        prime(&mut c);
        // 1–3 帧在途（低于 backlog_frames=4）持续 20 个 tick：既不降也不该判健康到升。
        for _ in 0..20 {
            let d = c.on_feedback(fb(50, 10, 3), 0, 0);
            assert_ne!(d.action, QosAction::Decrease, "3 帧在途是流水线，不是积压");
            assert_eq!(d.rates.bitrate_kbps, 6000, "码率不该被这种噪声压低");
        }
        // 写队列 1/4 满同样不算。
        for _ in 0..20 {
            let d = c.on_feedback(fb(50, 10, 0), 1, 0);
            assert_ne!(d.action, QosAction::Decrease, "写队列只排了 1 帧不算积压");
        }
    }

    #[test]
    fn single_backlog_tick_is_not_congestion() {
        let mut c = balanced();
        // 单次深积压（rtt 正常）：不算拥塞（需连续 2 个 tick）。
        let d = c.on_feedback(fb(50, 10, 6), 0, 0);
        assert_eq!(d.action, QosAction::Hold, "单次积压抖动不该触发降码率");
        // 第二次连续积压 → 判拥塞。
        let d = c.on_feedback(fb(50, 10, 6), 0, 0);
        assert_eq!(d.action, QosAction::Decrease);
        assert_eq!(d.rates.bitrate_kbps, 4800);
    }

    #[test]
    fn local_backlog_counts_toward_congestion() {
        let mut c = balanced();
        // 反馈里的积压为 0，但本地出站队列有积压，连续两 tick → 拥塞。
        c.on_feedback(fb(50, 10, 0), 3, 0);
        let d = c.on_feedback(fb(50, 10, 0), 3, 0);
        assert_eq!(d.action, QosAction::Decrease);
    }

    #[test]
    fn decreases_then_floors_bitrate_then_fps() {
        let mut c = balanced();
        prime(&mut c);
        // 持续拥塞：先把码率压到地板。
        let mut guard = 0;
        while c.current().bitrate_kbps > 200 && guard < 100 {
            c.on_feedback(fb(300, 10, 0), 0, 0);
            guard += 1;
        }
        assert_eq!(c.current().bitrate_kbps, 200, "码率应压到地板");
        assert_eq!(c.current().fps, 30, "码率未到底前不应动帧率");

        // 继续拥塞：帧率逐 tick 下降，地板是 15（不是 5）。
        for _ in 0..40 {
            c.on_feedback(fb(300, 10, 0), 0, 0);
        }
        assert_eq!(c.current().fps, 15, "帧率地板应为 15");
        assert!((c.current().scale - 1.0).abs() < f32::EPSILON, "解码不慢时不降分辨率");
    }

    #[test]
    fn fps_floor_is_fifteen_not_five() {
        let mut c = balanced();
        prime(&mut c);
        // 远超降到地板所需的 tick 数。
        for _ in 0..200 {
            c.on_feedback(fb(300, 10, 0), 0, 0);
        }
        assert_eq!(c.params.min_fps, 15);
        assert_eq!(c.current().fps, 15, "帧率不得跌破 15");
    }

    #[test]
    fn stable_high_rtt_is_not_congestion_and_recovers() {
        let mut c = balanced();
        // 基线 200ms：跨地域中继的常态，本身不该被判拥塞。
        let d = c.on_feedback(fb(200, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Hold);
        // 稳定在 205ms、队列空：绝不降级（在封顶时动作为 Hold）。
        for _ in 0..20 {
            let d = c.on_feedback(fb(205, 10, 0), 0, 0);
            assert_ne!(d.action, QosAction::Decrease, "稳定在基线附近不应降级");
            assert_eq!(d.rates.bitrate_kbps, 6000, "码率不该被压低");
        }

        // 真正拥塞后（相对基线持续抬升）能恢复：先降一档，再让 RTT 回到基线附近。
        feed_until(&mut c, fb(500, 10, 0), QosAction::Decrease);
        let low = c.current().bitrate_kbps;
        assert!(low < 6000);
        // 平滑值需要 1–2 个 tick 才衰减回阈值以下：余温最多再降一档，之后必须转为回升。
        let mut decreases = 0;
        for _ in 0..6 {
            if c.on_feedback(fb(205, 10, 0), 0, 0).action == QosAction::Decrease {
                decreases += 1;
            }
        }
        assert!(decreases <= 1, "RTT 回落后余温最多再降一档，实际 {decreases}");
        assert!(c.current().bitrate_kbps > low, "应能回升");
    }

    #[test]
    fn rtt_spike_above_baseline_is_congestion() {
        let mut c = balanced();
        prime(&mut c); // 基线 50ms
        // 基线 50 + 容限 120 = 170，400ms 已显著抬升（但要平滑 + 连续 tick 才动作）。
        let ticks = feed_until(&mut c, fb(400, 10, 0), QosAction::Decrease);
        assert!(ticks <= 4, "持续 400ms 应在 3–4 个 tick 内判拥塞，实际 {ticks}");
        assert_eq!(c.current().bitrate_kbps, 4800);

        // 高基线同理：200ms 基线 + 120 容限 = 320，400ms 仍判拥塞。
        let mut c = balanced();
        c.on_feedback(fb(200, 10, 0), 0, 0);
        let ticks = feed_until(&mut c, fb(400, 10, 0), QosAction::Decrease);
        assert!(ticks <= 5, "高基线下持续 400ms 也应判拥塞，实际 {ticks}");
        assert_eq!(c.current().bitrate_kbps, 4800);
    }

    #[test]
    fn zero_rtt_samples_do_not_poison_the_baseline() {
        let mut c = balanced();
        // 控制器在首个心跳 ack 前会发 rtt_ms=0（「尚无样本」哨兵）：既不算拥塞也不算健康，
        // 更不能把基线学成 0。
        for _ in 0..10 {
            let d = c.on_feedback(fb(0, 0, 0), 0, 0);
            assert_eq!(d.action, QosAction::Hold, "无样本时不该升也不该降");
        }
        assert_eq!(c.baseline_rtt_ms(), None, "0 不参与基线学习");

        // 首个真实样本建立 200ms 基线；随后稳定 205ms 不再被误判拥塞。
        c.on_feedback(fb(200, 10, 0), 0, 0);
        assert_eq!(c.baseline_rtt_ms(), Some(200));
        for _ in 0..10 {
            let d = c.on_feedback(fb(205, 10, 0), 0, 0);
            assert_ne!(d.action, QosAction::Decrease, "稳定中继链路不应降级");
        }
    }

    #[test]
    fn baseline_tracks_session_minimum_rtt() {
        let mut c = balanced();
        assert_eq!(c.baseline_rtt_ms(), None);
        c.on_feedback(fb(180, 10, 0), 0, 0);
        assert_eq!(c.baseline_rtt_ms(), Some(180));
        c.on_feedback(fb(140, 10, 0), 0, 0);
        assert_eq!(c.baseline_rtt_ms(), Some(140), "更低样本更新基线");
        c.on_feedback(fb(220, 10, 0), 0, 0);
        assert_eq!(c.baseline_rtt_ms(), Some(140), "基线只降不升");
        // 0 是「尚无样本」哨兵，不参与基线。
        c.on_feedback(fb(0, 10, 0), 0, 0);
        assert_eq!(c.baseline_rtt_ms(), Some(140));
    }

    #[test]
    fn resolution_drops_only_when_decode_is_the_bottleneck() {
        // 解码不慢（10ms）：即使码率/帧率都到地板也不降分辨率。
        let mut c = balanced();
        prime(&mut c);
        for _ in 0..80 {
            let d = c.on_feedback(fb(300, 10, 0), 0, 0);
            assert!(!d.force_keyframe, "解码不慢时不应降分辨率");
        }
        assert!((c.current().scale - 1.0).abs() < f32::EPSILON);

        // 解码慢（40ms，落进 4K 实测 37–58ms 区间）：码率/帧率到底后降分辨率。
        let mut c = balanced();
        prime(&mut c);
        let mut saw_scale_drop = false;
        for _ in 0..80 {
            let d = c.on_feedback(fb(300, 40, 0), 0, 0);
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
        let mut c = balanced();
        prime(&mut c);
        // 压到地板 + 帧率下限。
        for _ in 0..80 {
            c.on_feedback(fb(300, 10, 0), 0, 0);
        }
        assert_eq!(c.current().bitrate_kbps, 200);
        assert_eq!(c.current().fps, 15);

        // 健康：码率逐 tick 回升；**在码率回到上限之前，帧率不得恢复**（否则会来回抖）。
        let mut ticks = 0;
        while c.current().bitrate_kbps < 6000 {
            let d = c.on_feedback(fb(50, 10, 0), 0, 0);
            assert_eq!(d.rates.fps, 15, "码率回到上限前不应恢复帧率");
            ticks += 1;
            assert!(ticks < 200, "码率回升不应无限进行");
        }
        // 码率到顶后，帧率开始回升。
        let d = c.on_feedback(fb(50, 10, 0), 0, 0);
        assert_eq!(d.rates.fps, 16, "码率到顶后应恢复帧率");
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
                let d = c.on_feedback(f, 0, 0);
                assert!(d.rates.bitrate_kbps >= 200 && d.rates.bitrate_kbps <= 2000);
                assert!(d.rates.fps >= 15 && d.rates.fps <= 20);
                assert!(d.rates.scale >= 0.25 - f32::EPSILON && d.rates.scale <= 0.5 + f32::EPSILON);
            }
        }
    }

    #[test]
    fn set_ceiling_resets_to_new_ceiling() {
        let mut c = balanced();
        prime(&mut c);
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        assert!(c.current().bitrate_kbps < 6000);
        c.set_ceiling(QualityPreset::Hd.options());
        assert_eq!(
            c.current(),
            EncodeRates {
                bitrate_kbps: 10_000,
                fps: 60,
                scale: 1.0
            },
            "换档位应把生效值重置到新上限"
        );
    }

    #[test]
    fn frame_rate_ceiling_selects_60_then_30() {
        let mut c = balanced(); // 档位上限 30
        prime(&mut c);
        // 先让闭环降一点码率，验证 `set_frame_rate_ceiling` 不动它。
        let d = c.on_feedback(fb(300, 10, 0), 0, 0);
        assert_eq!(d.action, QosAction::Hold, "平滑后单条尖峰不动作");
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        let bitrate = c.current().bitrate_kbps;
        let scale = c.current().scale;

        c.set_frame_rate_ceiling(60);
        assert_eq!(c.ceiling().max_fps, 60, "上限应升到 60");
        assert_eq!(c.current().fps, 60, "生效帧率同步到新上限");
        assert_eq!(c.current().bitrate_kbps, bitrate, "码率不受帧率选择影响");
        assert!((c.current().scale - scale).abs() < f32::EPSILON, "缩放不受影响");

        // 选回 30：上限与生效值都降回去（仍是用户显式选择，不留 60 的闭环历史）。
        c.set_frame_rate_ceiling(30);
        assert_eq!(c.ceiling().max_fps, 30);
        assert_eq!(c.current().fps, 30);
        assert_eq!(c.current().bitrate_kbps, bitrate, "码率仍不受影响");
    }

    #[test]
    fn frame_rate_ceiling_is_clamped_to_min_fps() {
        let mut c = balanced();
        c.set_frame_rate_ceiling(5);
        assert_eq!(c.params.min_fps, 15);
        assert_eq!(c.ceiling().max_fps, 15, "低于 min_fps 的选择应被钳到 15");
        assert_eq!(c.current().fps, 15);

        c.set_frame_rate_ceiling(255);
        assert_eq!(c.ceiling().max_fps, 120, "上限封顶 120");
        assert_eq!(c.current().fps, 120);
    }

    #[test]
    fn frame_rate_ceiling_survives_after_ceiling_reset() {
        // 模拟 controlled.rs 的 SetQuality 处理顺序：换档位后重新应用用户选的帧率。
        let mut c = balanced();
        c.set_frame_rate_ceiling(60);
        c.set_ceiling(QualityPreset::Smooth.options()); // 档位默认 20
        assert_eq!(c.current().fps, 20, "换档位本身会回到档位默认");
        c.set_frame_rate_ceiling(60);
        assert_eq!(c.current().fps, 60, "重新应用后用户选择仍生效");
        assert_eq!(c.ceiling().target_bitrate_kbps, 2000, "码率仍是档位默认");
    }

    #[test]
    fn congestion_beats_health_when_both_signals_present() {
        // rtt 极低但队列有深积压：拥塞判定优先（不能一边积压一边升码率）。
        let mut c = balanced();
        c.on_feedback(fb(50, 10, 6), 0, 0);
        let d = c.on_feedback(fb(50, 10, 6), 0, 0);
        assert_eq!(d.action, QosAction::Decrease);
    }

    /// 内容上限的「收紧」要连续 `cap_hold_ticks` 次：先配一个可预期的迟滞值。
    fn activity_controller() -> RateController {
        RateController::with_params(QualityPreset::Balanced.options(), QosParams::default())
    }

    #[test]
    fn ui_activity_caps_effective_bitrate_and_fps() {
        let mut c = activity_controller();
        prime(&mut c);
        // 初始等级是 Active（无上限）；UI 面积是**降档**，需要连续 2 次（默认迟滞）。
        c.observe_activity(10);
        assert_eq!(c.current().bitrate_kbps, 6000, "单次安静观测不该收紧上限");
        c.observe_activity(0);
        assert_eq!(c.current().bitrate_kbps, 1200, "UI 面积应压到 ui_bitrate_cap_kbps");
        assert_eq!(c.current().fps, 15, "UI 面积应压到 ui_fps_cap");
        assert_eq!(c.content_cap_kbps(), Some(1200));
        assert_eq!(c.content_cap_fps(), Some(15));
    }

    #[test]
    fn active_area_releases_cap_immediately() {
        let mut c = activity_controller();
        prime(&mut c);
        c.observe_activity(0);
        c.observe_activity(0);
        assert_eq!(c.current().bitrate_kbps, 1200, "先进入 UI 档");

        // 面积变大（视频/拖窗）：**立刻**放开，不等迟滞。
        c.observe_activity(1000);
        assert_eq!(c.current().bitrate_kbps, 6000, "面积变大应立刻回到 AIMD 水平");
        assert_eq!(c.current().fps, 30);
        assert_eq!(c.content_cap_kbps(), None);
        let d = c.on_feedback(fb(50, 10, 0), 0, 0);
        assert!(!d.force_keyframe, "上限变化绝不置关键帧");
    }

    #[test]
    fn shrinking_area_needs_consecutive_quiet_ticks() {
        let mut c = activity_controller();
        prime(&mut c);
        let ticks = c.params.cap_hold_ticks;
        assert!(ticks >= 2, "默认迟滞应至少 2 tick");

        // 视频停下来的第一秒：仍然是满额（避免视频里的静止帧把上限压死）。
        c.observe_activity(0);
        assert_eq!(c.current().bitrate_kbps, 6000);
        // 中途来一帧大变化：连续安静计数被清零。
        c.observe_activity(1000);
        c.observe_activity(0);
        assert_eq!(c.current().bitrate_kbps, 6000, "安静计数被大变化打断后要重新数");

        // 还差一次才到迟滞次数：仍不收紧。
        for _ in 1..ticks.saturating_sub(1) {
            c.observe_activity(0);
            assert_eq!(c.current().bitrate_kbps, 6000, "未达 cap_hold_ticks 时不得收紧");
        }
        // 第 ticks 次连续安静：收紧。
        c.observe_activity(0);
        assert_eq!(c.current().bitrate_kbps, 1200, "连续安静达到迟滞次数后收紧");
    }

    #[test]
    fn content_cap_never_exceeds_quality_ceiling() {
        // 上限配得比档位上限还大：生效值仍不得越过档位（Smooth：2000 kbps / 20 fps）。
        let params = QosParams {
            ui_bitrate_cap_kbps: 8000,
            ui_fps_cap: 60,
            cap_hold_ticks: 1,
            ..QosParams::default()
        };
        let mut c = RateController::with_params(QualityPreset::Smooth.options(), params);
        prime(&mut c);
        lo_activity_twice(&mut c);
        assert_eq!(c.current().bitrate_kbps, 2000, "内容上限不得越过档位上限");
        assert_eq!(c.current().fps, 20, "帧率上限不得越过档位上限");
    }

    /// 用 UI 面积把上限收紧（默认迟滞下调用两次）。
    fn lo_activity_twice(c: &mut RateController) {
        c.observe_activity(0);
        c.observe_activity(0);
    }

    #[test]
    fn content_cap_never_requests_keyframe_or_changes_scale() {
        let mut c = activity_controller();
        prime(&mut c);
        lo_activity_twice(&mut c);
        assert_eq!(c.current().bitrate_kbps, 1200);
        assert!(
            (c.current().scale - 1.0).abs() < f32::EPSILON,
            "面积自适应只动码率/帧率，不动分辨率"
        );
        // 后续一次健康反馈：动作可以是 Increase/Hold，但**绝不**因上限变化置关键帧。
        for _ in 0..5 {
            let d = c.on_feedback(fb(50, 10, 0), 0, 0);
            assert!(!d.force_keyframe, "内容上限绝不带出 IDR");
            assert!((d.rates.scale - 1.0).abs() < f32::EPSILON);
        }
        // AIMD 状态没被上限改写：放开面积后立刻回到上限。
        c.observe_activity(1000);
        assert_eq!(c.current().bitrate_kbps, 6000);
        assert_eq!(c.current().fps, 30);
    }

    // ───────────────── 解码瓶颈 vs 网络拥塞（阶段 4） ─────────────────

    #[test]
    fn decode_bound_reduces_fps_before_bitrate() {
        let mut c = RateController::new(QualityPreset::Hd.options()); // 10000 kbps / 60 fps
        c.on_feedback(fb(50, 10, 0), 0, 0); // 建立 RTT 基线；健康且已在封顶 → Hold
        // decode_ms=20 @60fps → 占用率 1.2（单帧已解不及），且 backlog>0 → 判饱和。
        let d = c.on_feedback(fb(50, 20, 1), 0, 0);
        assert_eq!(d.action, QosAction::Hold, "第一个饱和 tick 属于迟滞期");
        let d = c.on_feedback(fb(50, 20, 1), 0, 0);
        assert_eq!(d.action, QosAction::Decrease);
        // 乘性下降 ×3/4：60 → 45（不是 -1）。饱和是「现在就解不动」，必须比网络路径更快收敛。
        assert_eq!(d.rates.fps, 45, "解码瓶颈应**先降帧率**且乘性下降");
        assert_eq!(d.rates.bitrate_kbps, 10_000, "解码瓶颈不得先砍码率");
        assert!(!d.force_keyframe, "改帧率不是几何变化，不该插 IDR");

        // 连续饱和（60ms/帧，降到 15fps 后仍是 0.9 ≥ 0.8）：
        // 60 → 45 → 33 → 24 → 18 → 15，5 个 tick 落到地板（旧的 -1/tick 要 45 个 tick）。
        let mut ticks = 0;
        while c.current().fps > 15 {
            c.on_feedback(fb(50, 60, 1), 0, 0);
            ticks += 1;
            assert!(ticks < 10, "乘性下降应在个位数 tick 内到地板");
        }
        assert_eq!(c.current().fps, 15);
        assert_eq!(c.current().bitrate_kbps, 10_000, "降帧率期间不得砍码率");
    }

    #[test]
    fn decode_bound_eventually_drops_scale_and_requests_keyframe() {
        let mut c = RateController::new(QualityPreset::Hd.options());
        c.on_feedback(fb(50, 10, 0), 0, 0);
        // 60ms @ 60fps = 3.6，降到 15fps 后是 0.9（≥ 预算 0.8）；队列**持续深积压**
        // （6 帧 ≥ `backlog_frames`）说明解码器确实追不上，故始终判饱和。
        let mut saw_scale_drop = false;
        for _ in 0..120 {
            let d = c.on_feedback(fb(50, 60, 6), 0, 0);
            if d.force_keyframe {
                saw_scale_drop = true;
                break;
            }
        }
        assert!(saw_scale_drop, "帧率降到下限后应降分辨率并强制关键帧");
        assert!(c.current().scale < 1.0, "分辨率应下降");
    }

    /// **网络拥塞绝不改几何**：分辨率是用户第一眼的观感，改几何要重建编码器 + 强制 IDR，
    /// 恢复又排在码率/帧率之后（真机上表现为「1080p 几秒内掉到 342p」）。持续拥塞只该压
    /// 码率与帧率。
    #[test]
    fn network_congestion_never_drops_resolution() {
        let mut c = RateController::new(QualityPreset::Hd.options()); // 10000 kbps / 60 fps
        prime(&mut c);
        for _ in 0..120 {
            let d = c.on_feedback(fb(400, 10, 0), 0, 0);
            assert!(!d.force_keyframe, "网络拥塞不得降分辨率/插 IDR");
            assert!((d.rates.scale - 1.0).abs() < f32::EPSILON, "网络阶梯不动分辨率");
        }
        assert_eq!(c.current().bitrate_kbps, 200, "码率停在地板");
        assert_eq!(c.current().fps, 15, "阶梯到底后停在帧率下限");
    }

    /// 掉到地板后必须**快速**回升：纯加性 +10% 从 200 回到 10 Mbps 要 41 个 tick，
    /// 用户看到的是「掉下去一分钟才回来」。
    #[test]
    fn recovers_from_floor_within_a_few_ticks() {
        let mut c = RateController::new(QualityPreset::Hd.options());
        prime(&mut c);
        for _ in 0..120 {
            c.on_feedback(fb(400, 10, 0), 0, 0);
        }
        assert_eq!(c.current().bitrate_kbps, 200);
        let mut ticks = 0;
        while c.current().bitrate_kbps < 10_000 {
            c.on_feedback(fb(50, 10, 0), 0, 0);
            ticks += 1;
            assert!(ticks < 40, "从地板回到上限不该超过 40 个 tick");
        }
        assert!(ticks <= 25, "快升阶段应在二十来个 tick 内回到上限，实际 {ticks}");
    }

    /// 抖动链路（样本在基线上下摆动、长期均值贴基线）不该累积成持续拥塞。
    #[test]
    fn rtt_jitter_does_not_collapse_bitrate() {
        let mut c = RateController::new(QualityPreset::Hd.options());
        // 基线 100ms；样本在 100/260 之间交替（旧判据单条 260 > 220 就降一档）。
        let seq = [100, 260, 110, 250, 105, 240, 120, 230];
        for _ in 0..20 {
            for rtt in seq {
                let d = c.on_feedback(fb(rtt, 10, 0), 0, 0);
                assert!(!d.force_keyframe, "抖动不该降分辨率");
            }
        }
        assert!(
            c.current().bitrate_kbps >= 6000,
            "抖动不该把码率压垮：{}",
            c.current().bitrate_kbps
        );
    }

    /// 接近容量运行（队列里持续有一两帧在途）时链路仍应判健康、可以回升。
    #[test]
    fn shallow_queue_still_allows_recovery() {
        let mut c = balanced();
        prime(&mut c);
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        let low = c.current().bitrate_kbps;
        // 队列里持续 2 帧在途（流水线常态）、RTT 贴基线 → 应升而不是卡死。
        let d = c.on_feedback(fb(50, 10, 2), 0, 0);
        assert_eq!(d.action, QosAction::Increase);
        assert!(d.rates.bitrate_kbps > low);
    }

    #[test]
    fn delivery_ratio_below_threshold_needs_three_ticks() {
        let mut c = balanced();
        prime(&mut c);
        // RTT 健康、无积压，但出 1000 kbps 只收到 500 kbps（0.5 < 0.7）。
        let bad = || Feedback {
            rtt_ms: 50,
            recv_kbps: 500,
            decode_ms: 10,
            backlog: 0,
        };
        let d = c.on_feedback(bad(), 0, 1000);
        assert_eq!(d.action, QosAction::Hold, "1 tick 投递率差是噪声");
        let d = c.on_feedback(bad(), 0, 1000);
        assert_eq!(d.action, QosAction::Hold, "2 tick 仍未到阈值");
        let d = c.on_feedback(bad(), 0, 1000);
        assert_eq!(d.action, QosAction::Decrease, "连续 3 tick 投递率 <0.7 判网络拥塞");
        assert_eq!(d.rates.bitrate_kbps, 4800);
    }

    #[test]
    fn delivery_ratio_ignores_zero_samples() {
        // recv_kbps = 0（控制器尚无接收样本）：即便 sent 很高也不得判丢包。
        let mut c = balanced();
        prime(&mut c);
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease); // 制造可回升的空间
        let low = c.current().bitrate_kbps;
        let d = c.on_feedback(
            Feedback {
                rtt_ms: 50,
                recv_kbps: 0,
                decode_ms: 10,
                backlog: 0,
            },
            0,
            1000,
        );
        assert_eq!(d.action, QosAction::Increase, "recv_kbps=0 是「无样本」哨兵");
        assert!(d.rates.bitrate_kbps > low, "健康时应正常回升");

        // sent_kbps = 0（编码任务尚未上报窗口字节）：同样不得触发投递率路径。
        let mut c = balanced();
        prime(&mut c);
        feed_until(&mut c, fb(300, 10, 0), QosAction::Decrease);
        let low = c.current().bitrate_kbps;
        let d = c.on_feedback(
            Feedback {
                rtt_ms: 50,
                recv_kbps: 500,
                decode_ms: 10,
                backlog: 0,
            },
            0,
            0,
        );
        assert_eq!(d.action, QosAction::Increase, "sent_kbps=0 是「无样本」哨兵");
        assert!(d.rates.bitrate_kbps > low, "健康时应正常回升");
    }
}
