//! GUI 专属偏好持久化（gui.toml，与 client 配置同目录、互不干扰）。

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use rust_tunnel_remote::{ApprovalPolicy, QualityPreset};

use crate::config_store;
use crate::remote::{ViewScale, WindowSize};

/// 布尔字段的 `serde(default = ...)` 目标：缺省为 true。
///
/// 不能对「默认应为 true」的 bool 直接用裸 `#[serde(default)]`——`bool::default()` 是 false，
/// 旧配置文件缺字段时会静默落到 false（对「自动重连」这类默认开启的行为是反直觉的）。
fn default_true() -> bool {
    true
}

/// 远程控制帧率上限的缺省值（30 fps）。
fn default_remote_frame_rate() -> u8 {
    rust_tunnel_remote::DEFAULT_FRAME_RATE
}

/// 控制端播放音量的默认值（100%）。
fn default_audio_volume() -> u8 {
    100
}

/// 放大锐化强度的默认百分比（引用 `nv12_renderer` 的单一事实来源，避免两处各写一个 42）。
fn default_sharpen_strength() -> u8 {
    crate::remote::nv12_renderer::CAS_SHARPEN_DEFAULT_PERCENT
}

/// 把读到的帧率钳到合法范围 `15..=60`。
///
/// UI 当前只提供 30 / 60（后续可能扩到 15），但手改 `gui.toml` 可能写入任意值；
/// 0 之类的越界值会让采集间隔退化成 1 fps，故读取时统一收敛。
fn normalize_remote_frame_rate(fps: u8) -> u8 {
    fps.clamp(15, 60)
}

/// 主题偏好（持久化值；egui 的 `ThemePreference` 未启用 serde，自行映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    /// 跟随系统。
    #[default]
    System,
    /// 浅色。
    Light,
    /// 深色。
    Dark,
}

impl ThemePref {
    /// 循环切换：跟随系统 → 浅色 → 深色 → 跟随系统。
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    /// 映射为 egui 主题偏好。
    #[must_use]
    pub fn to_egui(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    /// 中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }
}

/// 放大增强算法（控制端本机上屏；仅画面被**放大**时生效，1:1 / 缩小都回退双线性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum UpscaleMode {
    /// FSR1（EASU + RCAS）：边缘自适应放大 + 对比度自适应锐化。默认。
    #[default]
    #[serde(rename = "fsr")]
    Fsr,
    /// 双线性 + CAS：旧的放大 + 仅亮度平面锐化。
    #[serde(rename = "cas")]
    Cas,
}

impl UpscaleMode {
    /// 全部选项（设置页下拉顺序）。
    pub const ALL: [Self; 2] = [Self::Fsr, Self::Cas];

    /// 中文标签（设置页下拉）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fsr => "FSR1（EASU+RCAS）",
            Self::Cas => "双线性 + CAS",
        }
    }
}

/// GUI 偏好根结构（serde(default) 保证前向兼容）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "GUI 偏好本就是一组互不相关的独立开关，拆成状态机只会更难读"
)]
pub struct GuiPrefs {
    /// 主题偏好。
    #[serde(default)]
    pub theme: ThemePref,
    /// 是否允许被其他客户端远程控制。**默认关闭**：开启等于把本机屏幕与输入
    /// 交给持有效 token 的对端，必须是用户的显式选择，且随进程重启保留。
    #[serde(default)]
    pub allow_remote: bool,
    /// 被控端的接入确认策略。默认 `Confirm`——旧配置文件缺此字段时也落到「每次询问」，
    /// 这是安全侧默认，不要改成 `Auto`。
    #[serde(default)]
    pub remote_approval: ApprovalPolicy,
    /// 被控端**会话结束后自动锁定本机屏幕**。
    ///
    /// **默认关闭**：它会在无人值守时把本机锁在登录界面；而且 GUI 模式被控端通常无法采集
    /// Windows 安全桌面，锁屏后远程画面即中断（要远程解锁需无人值守服务实例）。必须是用户
    /// 的显式选择。旧配置文件缺此字段时落到 `false`。
    #[serde(default)]
    pub remote_auto_lock_on_disconnect: bool,
    /// 远程会话窗口：工具条是否自动隐藏（指针静止后淡出，移到上缘或移动指针再现）。
    ///
    /// **默认关闭**：工具条是会话里唯一的操作入口，默认常驻最省心；要淡出的人可开。
    /// 注意 `save()` 会序列化每个字段，所以**现有用户的 `gui.toml` 早已把旧默认 `true` 钉死**，
    /// 改这里的默认值只对**全新配置**生效——真正让老用户能关掉它的是工具条上的「自动隐藏」开关
    /// （见 `ui::session_window::toolbar`），不是这个默认值。
    #[serde(default)]
    pub remote_toolbar_autohide: bool,
    /// 远程会话窗口：悬浮工具条是否**收起到只显示延迟**。
    ///
    /// 默认展开。收起是显式选择，跨会话与重启保留；自动隐藏（`remote_toolbar_autohide`）
    /// 只作用于展开态——收起态已经足够小，再淡出就看不到延迟了。
    #[serde(default)]
    pub remote_toolbar_collapsed: bool,
    /// 一次性迁移标志：`remote_toolbar_autohide` 的旧默认 `true` 是否已强制改为 `false`。
    ///
    /// 缺键读作 `false`（= 尚未迁移），见 `migrate_toolbar_autohide`。置位后**永不再迁移**，
    /// 否则用户手动重新打开「自动隐藏」会被每次启动悄悄改回去。
    #[serde(default)]
    pub remote_toolbar_autohide_migrated: bool,
    /// 远程会话窗口：画面缩放档位（持久化的是上次选择，不是每场的运行时值）。
    #[serde(default)]
    pub remote_view_scale: ViewScale,
    /// 远程会话窗口：窗口尺寸档位。
    #[serde(default)]
    pub remote_window_size: WindowSize,
    /// 剪贴板自动同步。**默认关闭**：自动同步会把本机剪贴板内容（可能含密码、token）悄悄发给
    /// 对端，必须是用户的显式选择，且只在本端主动开启的会话里生效。
    #[serde(default)]
    pub remote_clipboard_sync: bool,
    /// 断线自动重连。默认开启：会话因断线（非主动结束）时按退避重试，用户可随时停止。
    #[serde(default = "default_true")]
    pub remote_auto_reconnect: bool,
    /// 中继会话**自动尝试升级到 P2P 打洞**。**默认开启**。
    ///
    /// 会话跑在 hbbr 中继上时，后台每 30 s 尝试一次打洞；成功后把数据面**无缝**换到 P2P
    /// （协议 v12 的会话内路径迁移）。提交点（`PathFreeze`）之前的失败完全无害——会话照常
    /// 走中继；提交点之后的失败按会话故障处理（走既有自动重连）。关闭后会话一直留在中继。
    #[serde(default = "default_true")]
    pub remote_p2p_upgrade: bool,
    /// 会话窗口内的统计角标（延迟 / 路径 / 分辨率 / 帧率 / 时长）是否常显。
    #[serde(default)]
    pub remote_stats_overlay: bool,
    /// 统计角标是否显示**详细指标**（链路 / 被控端 / 画面三组）。
    ///
    /// 默认关闭：简洁档只留一眼要看的十行（延迟 / 路径 / 编码器 / 采集 / 耗时 / 帧率 / 码率 /
    /// 端到端 / 时长）；详细档字段多、占屏面积大，是排查问题时的显式选择。
    #[serde(default)]
    pub remote_stats_detail: bool,
    /// 远程控制帧率**上限**（30 / 60；默认 30）。
    ///
    /// 帧率是上限而非固定值：QoS 闭环仍可在其下降级（地板 15）。这里是控制端的偏好，
    /// 会话建立时自动下发一次。
    #[serde(default = "default_remote_frame_rate")]
    pub remote_frame_rate: u8,
    /// 远程控制画质档位（流畅 / 均衡 / 高清；默认 Balanced）。
    ///
    /// 档位是**上限**，实际码率/帧率仍由 QoS 闭环在上限之下浮动。会话建立时自动下发一次。
    #[serde(default)]
    pub remote_quality: QualityPreset,
    /// 被控端是否使用**原生分辨率**（不缩放到 1920 宽）编码。**默认开启**。
    ///
    /// 只在协商到**硬件 H.264** 时生效：编码宽度上限从 1920 放宽到 3840（显示宽度 ≤1920
    /// 时不产生任何变化），档位码率翻倍（封顶 30 Mbps）。代价是控制器的解码/上屏压力与
    /// 上行峰值随像素增长——但 4K 桌面缩到 1920 再上屏，任何码率都换不回文字细节，这是
    /// 现场「字看不清」的直接原因。VP9 路径不受影响。运行中修改只对下一次会话生效。
    ///
    /// 旧配置由 [`migrate_native_resolution`] 一次性改为开启。
    #[serde(default = "default_true")]
    pub remote_native_resolution: bool,
    /// [`GuiPrefs::remote_native_resolution`] 的默认值迁移标记（只跑一次）。
    #[serde(default)]
    pub remote_native_resolution_migrated: bool,
    /// 是否**优先协商 H.265/HEVC**（双方都支持时）。**默认开启**。
    ///
    /// 只有双方硬件/系统扩展都具备时才真的走 H.265；任一侧缺能力、或设置了
    /// `RUST_TUNNEL_DISABLE_H265`（任意非空值）都自动回退 H.264/VP9。运行中修改只对
    /// **下一次会话**生效（引擎在会话建立时读一次）。
    #[serde(default = "default_true")]
    pub remote_prefer_h265: bool,
    /// 是否**优先 4:4:4（VP9 profile 1）**编码（默认关闭）。
    ///
    /// 开启后即使双方都支持 H.264/H.265 也优先协商 VP9：VP9 的 I444 色度不子采样，
    /// ClearType 文字的彩边最少；代价是软件编码、CPU 占用更高（4K 不建议开启）。
    /// 优先级高于 [`GuiPrefs::remote_prefer_h265`]。只在**被控端**生效，运行中修改只对
    /// **下一次会话**生效（引擎在会话建立时读一次）。
    #[serde(default)]
    pub remote_prefer_444: bool,
    /// 控制端上屏的**放大锐化**（CAS，只作用于亮度平面；仅在画面被放大时生效）。
    ///
    /// **默认开启**：1080p 远端在 4K 屏全屏时是 2× 放大，GPU 双线性插值会把文字明显抹糊。
    /// 锐化只在本机 GPU 上屏着色器里做，不上线、不影响对端；1:1 与缩小时自动不生效。
    /// 纯 CPU（Glow / VP9 纹理）路径不经过该着色器，改了也没有效果。
    #[serde(default = "default_true")]
    pub remote_sharpen: bool,
    /// 放大锐化的**强度**（百分比 0–100；默认 42）。只在 `remote_sharpen` 开启、且画面确实被
    /// 放大时生效；0 等于关闭。用百分比 u8 而非 f32，既避免浮点 serde 的精度/表示问题，也与
    /// `remote_audio_volume` 的风格一致。
    #[serde(default = "default_sharpen_strength")]
    pub remote_sharpen_strength: u8,
    /// 放大增强算法（FSR1 / 双线性+CAS）。默认 FSR1；缺键的旧配置也落到 FSR1。
    #[serde(default)]
    pub remote_upscale_mode: UpscaleMode,
    /// 控制端会话开始时是否请求被控端转发声音。**默认开启**（与「批准即转发」的策略一致）。
    ///
    /// 运行中改它由工具条扬声器开关完成：立即作用于当前会话，并作为下一次会话的默认值。
    #[serde(default = "default_true")]
    pub remote_audio_enabled: bool,
    /// 控制端本地播放音量（百分比，0–100；0 = 静音）。**默认 100**。
    ///
    /// 这是本地播放增益，不上线；对端采集与否由 `remote_audio_enabled` / 工具条控制。
    #[serde(default = "default_audio_volume")]
    pub remote_audio_volume: u8,
    /// 被控端是否允许向对端转发系统声音。**默认开启**。
    ///
    /// 关掉后本机不会打开 loopback 设备，对端会收到「本机已停止转发声音」。
    #[serde(default = "default_true")]
    pub remote_audio_allowed: bool,
    /// 远程控制**分阶段延迟诊断日志**开关（默认关闭）。
    ///
    /// 打开后引擎每秒输出一行阶段汇总（采集/编码/发送/网络/解码/上屏），慢帧另打明细，
    /// 用于定位「切标签/切文件夹为什么慢」。日志走 GUI 的「日志」页；**需在两端分别打开**。
    /// 运行中切换立即生效（[`rust_tunnel_remote::latency::set_mode`]），无需重启。
    #[serde(default)]
    pub remote_latency_log: bool,
    /// 延迟诊断日志是否**每帧一行**（量大；默认关闭 = 只打每秒汇总 + 慢帧）。
    #[serde(default)]
    pub remote_latency_log_frames: bool,
    /// 自动检查客户端更新。**默认开启**：服务端在注册时也会主动推送一次，此项控制的是
    /// 长连接期间的 6 小时轮询。关掉后仍可在设置页手动检查。
    #[serde(default = "default_true")]
    pub update_auto_check: bool,
    /// 用户选择「跳过此版本」的版本号；缺省 `None`。
    ///
    /// 只影响弹窗：设置页仍会显示「有新版本（已跳过）」并提供入口。新版本到来时
    /// 自更新逻辑会正常弹窗，不需要用户回来清除本字段。
    #[serde(default)]
    pub update_skipped_version: Option<String>,
}

impl Default for GuiPrefs {
    fn default() -> Self {
        Self {
            theme: ThemePref::default(),
            allow_remote: false,
            remote_approval: ApprovalPolicy::default(),
            remote_auto_lock_on_disconnect: false,
            remote_toolbar_autohide: false,
            remote_toolbar_collapsed: false,
            remote_toolbar_autohide_migrated: false,
            remote_view_scale: ViewScale::default(),
            remote_window_size: WindowSize::default(),
            remote_clipboard_sync: false,
            // **必须与字段上的 `#[serde(default = "default_true")]` 一致**：那一条只覆盖
            // 「配置文件缺键」，全新安装/没有配置文件走的是这里。两处不一致会让「文档说默认开、
            // 实际默认关」，断线后不重连。
            remote_auto_reconnect: true,
            remote_p2p_upgrade: true,
            remote_stats_overlay: false,
            remote_stats_detail: false,
            remote_frame_rate: default_remote_frame_rate(),
            remote_quality: QualityPreset::default(),
            // 与字段上的 `#[serde(default = "default_true")]` 保持一致（理由见那里）。
            remote_native_resolution: true,
            remote_native_resolution_migrated: false,
            // 与字段上的 `#[serde(default = "default_true")]` 保持一致。
            remote_prefer_h265: true,
            // `#[serde(default)]` 即 `false`：默认走 H.264 硬件编码（GPU 色度修复路径）。
            remote_prefer_444: false,
            // 与字段上的 `#[serde(default = "default_true")]` 保持一致（放大时默认锐化）。
            remote_sharpen: true,
            // 与字段上的 `#[serde(default = "default_sharpen_strength")]` 保持一致。
            remote_sharpen_strength: default_sharpen_strength(),
            // `#[serde(default)]` = `UpscaleMode::default()` = Fsr。
            remote_upscale_mode: UpscaleMode::default(),
            remote_audio_enabled: true,
            remote_audio_volume: default_audio_volume(),
            remote_audio_allowed: true,
            remote_latency_log: false,
            remote_latency_log_frames: false,
            update_auto_check: true,
            update_skipped_version: None,
        }
    }
}

/// GUI 偏好文件路径（与 client 配置同目录的 gui.toml）。
#[must_use]
pub fn path() -> PathBuf {
    config_store::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("gui.toml"), |dir| dir.join("gui.toml"))
}

/// 一次性迁移：把 `remote_toolbar_autohide` 的旧默认 `true` 强制改回 `false`。
///
/// **为什么可以放心改写用户的配置**：改动前这个字段**根本没有 UI 控件**，只能靠手改
/// `gui.toml` 设置，所以磁盘上存的 `true` 几乎只可能是旧默认值、而非刻意选择，强制关一次
/// 是对的。（真正让老用户能开关它的是工具条上的「自动隐藏」按钮。）
///
/// 由 `remote_toolbar_autohide_migrated` 保证只跑一次：迁移后立刻落盘该标志，此后即便用户
/// 重新打开「自动隐藏」，重启也不会再被改回去。
///
/// 纯函数、无 I/O、无 `cfg`，故可在任意平台单测。返回是否发生改动，由调用方决定是否落盘。
#[must_use]
pub fn migrate_toolbar_autohide(prefs: &mut GuiPrefs) -> bool {
    if prefs.remote_toolbar_autohide_migrated {
        return false;
    }
    prefs.remote_toolbar_autohide_migrated = true;
    prefs.remote_toolbar_autohide = false;
    true
}

/// 一次性迁移：把 `remote_native_resolution` 的旧默认 `false` 改为 `true`。
///
/// **为什么可以改写用户的配置**：旧默认是「不缩放的代价（解码/上行）由用户显式承担」，
/// 结果是 4K 桌面被缩到 1920 再上屏——文字细节在编码前就丢了，任何码率都救不回来。
/// 磁盘上的 `false` 几乎只可能是旧默认值（这个开关藏在一级设置页，且说明写着「仅 4K 屏」）。
/// 迁移后用户仍可在设置页关掉。
///
/// 由 `remote_native_resolution_migrated` 保证只跑一次。纯函数、无 I/O，便于单测。
/// 返回是否发生改动，由调用方决定是否落盘。
#[must_use]
pub fn migrate_native_resolution(prefs: &mut GuiPrefs) -> bool {
    if prefs.remote_native_resolution_migrated {
        return false;
    }
    prefs.remote_native_resolution_migrated = true;
    prefs.remote_native_resolution = true;
    true
}

/// 加载 GUI 偏好；文件缺失/读取/解析失败时返回默认值。
#[must_use]
pub fn load() -> GuiPrefs {
    let p = path();
    let Ok(content) = std::fs::read_to_string(&p) else {
        // 没有配置文件 = 全新安装：没有任何旧值需要迁移，直接把标志置位。否则「全新安装 →
        // 用户打开自动隐藏（落盘 autohide=true, migrated=false）→ 重启」会被迁移误伤改回去。
        return GuiPrefs {
            remote_toolbar_autohide_migrated: true,
            remote_native_resolution_migrated: true,
            ..GuiPrefs::default()
        };
    };
    match toml::from_str::<GuiPrefs>(&content) {
        Ok(mut prefs) => {
            // 手改配置可能写入越界帧率；读进来就钳到合法范围（UI 只提供 30/60，将来扩到 15）。
            prefs.remote_frame_rate = normalize_remote_frame_rate(prefs.remote_frame_rate);
            // 迁移必须立刻落盘，否则标志在磁盘上仍是 false，每次启动都会重跑。
            // 两个迁移都必须无条件执行（各自的标志负责只跑一次），不能用 `||` 短路。
            let toolbar_migrated = migrate_toolbar_autohide(&mut prefs);
            let native_migrated = migrate_native_resolution(&mut prefs);
            if toolbar_migrated || native_migrated {
                if let Err(e) = save(&prefs) {
                    tracing::warn!("GUI 偏好一次性迁移落盘失败（{}）：{e}", p.display());
                }
            }
            prefs
        }
        Err(e) => {
            tracing::warn!("GUI 偏好解析失败（{}），使用默认值：{e}", p.display());
            // 解析失败同样没有可信的旧值可迁移，标志置位。
            GuiPrefs {
                remote_toolbar_autohide_migrated: true,
                remote_native_resolution_migrated: true,
                ..GuiPrefs::default()
            }
        }
    }
}

/// 串行化 `gui.toml` 的写入。
///
/// GUI 线程（主题/展示档位）与托盘事件线程（窗口隐藏时的被控开关）都会写同一个文件，
/// 且 `save` 用的是固定名字的临时文件 + rename——两处并发写会互相截断。
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// 原子写入 GUI 偏好（临时文件 + rename，避免崩溃写半截）。
///
/// # Errors
/// 当序列化或文件写入失败时返回 `Err`。
pub fn save(prefs: &GuiPrefs) -> Result<(), config_store::StoreError> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    save_unlocked(prefs)
}

/// 托盘线程在**窗口隐藏时**直接翻转「允许被远程控制」并落盘。
///
/// 此时 `update` 不会运行（见 `tray_worker` 的模块文档），所以不能等 GUI 侧来写。
/// 先 `load` 再 `save`：`load` 可能触发一次性迁移并自行落盘，那时还不能持有锁，
/// 故两段式（`save` 内部才加锁）而不是全程持锁。
///
/// # Errors
/// 当偏好文件读取后的序列化或写入失败时返回 `Err`。
pub fn update_allow_remote(on: bool) -> Result<(), config_store::StoreError> {
    let mut prefs = load();
    prefs.allow_remote = on;
    save(&prefs)
}

fn save_unlocked(prefs: &GuiPrefs) -> Result<(), config_store::StoreError> {
    let p = path();
    let toml_str =
        toml::to_string(prefs).map_err(|e| config_store::StoreError::Save(e.to_string()))?;
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| config_store::StoreError::Save(e.to_string()))?;
        }
    }
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, toml_str).map_err(|e| config_store::StoreError::Save(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| config_store::StoreError::Save(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 延迟诊断日志开关能 TOML 往返（缺键时落到关闭）。
    #[test]
    fn latency_log_prefs_round_trip() {
        let prefs = GuiPrefs {
            remote_latency_log: true,
            remote_latency_log_frames: true,
            ..GuiPrefs::default()
        };
        let text = toml::to_string(&prefs).expect("序列化");
        let parsed: GuiPrefs = toml::from_str(&text).expect("反序列化");
        assert!(parsed.remote_latency_log);
        assert!(parsed.remote_latency_log_frames);

        // 旧配置缺键：默认关闭，不得因反序列化失败丢掉整份偏好。
        let legacy: GuiPrefs = toml::from_str("theme = \"dark\"\n").expect("旧配置应能解析");
        assert!(!legacy.remote_latency_log);
        assert!(!legacy.remote_latency_log_frames);
    }

    /// 旧配置（未迁移 + 旧默认 true）→ 迁移后标志置位、自动隐藏强制关闭。
    #[test]
    fn old_config_gets_migrated() {
        let mut prefs = GuiPrefs {
            remote_toolbar_autohide: true,
            ..GuiPrefs::default()
        };
        assert!(
            !prefs.remote_toolbar_autohide_migrated,
            "构造的应是未迁移态"
        );

        assert!(migrate_toolbar_autohide(&mut prefs), "旧配置应发生迁移");
        assert!(prefs.remote_toolbar_autohide_migrated);
        assert!(!prefs.remote_toolbar_autohide, "旧默认 true 应被强制关闭");
    }

    /// 已迁移 + 用户手动重新打开 → **完全不变**。这是保护用户刻意选择的关键性质。
    #[test]
    fn already_migrated_user_choice_is_untouched() {
        let mut prefs = GuiPrefs {
            remote_toolbar_autohide: true,
            remote_toolbar_autohide_migrated: true,
            ..GuiPrefs::default()
        };

        assert!(
            !migrate_toolbar_autohide(&mut prefs),
            "已迁移的文件不得再次迁移"
        );
        assert!(
            prefs.remote_toolbar_autohide,
            "用户重新打开的自动隐藏必须保留"
        );
        assert!(prefs.remote_toolbar_autohide_migrated);
    }

    /// 全新默认（未迁移 + false）→ 标志置位、取值保持 false。
    #[test]
    fn fresh_default_marks_migrated_without_touching_value() {
        let mut prefs = GuiPrefs::default();
        assert!(!prefs.remote_toolbar_autohide);

        assert!(migrate_toolbar_autohide(&mut prefs));
        assert!(prefs.remote_toolbar_autohide_migrated);
        assert!(!prefs.remote_toolbar_autohide);
    }

    /// TOML 往返：模拟改动前的 `gui.toml`（序列化形状**去掉**新标志键）→ 能解析、能迁移；
    /// 再解析迁移后落盘的内容 → 不再迁移。这条证明「立刻落盘」真的闭合了回路。
    #[test]
    fn legacy_gui_toml_migrates_once_and_persists() {
        // 改动前的序列化形状：拿今天的结构序列化后删掉新键那一行。
        let legacy_full = toml::to_string(&GuiPrefs {
            remote_toolbar_autohide: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let legacy = legacy_full
            .lines()
            .filter(|line| {
                !line
                    .trim_start()
                    .starts_with("remote_toolbar_autohide_migrated")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !legacy.contains("remote_toolbar_autohide_migrated"),
            "旧文件不应含新键"
        );

        let mut prefs: GuiPrefs = toml::from_str(&legacy).unwrap();
        assert!(!prefs.remote_toolbar_autohide_migrated, "缺键应读作未迁移");
        assert!(prefs.remote_toolbar_autohide, "旧文件保留的应是 true");
        assert!(migrate_toolbar_autohide(&mut prefs));

        // 落盘 → 重新解析：这次必须不再迁移。
        let saved = toml::to_string(&prefs).unwrap();
        let mut reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(
            reparsed.remote_toolbar_autohide_migrated,
            "迁移标志应已落盘"
        );
        assert!(
            !migrate_toolbar_autohide(&mut reparsed),
            "已迁移的文件不得再次迁移"
        );
        assert!(!reparsed.remote_toolbar_autohide);
    }

    #[test]
    fn frame_rate_defaults_to_thirty_and_survives_roundtrip() {
        assert_eq!(GuiPrefs::default().remote_frame_rate, 30, "新配置默认 30");
        // 缺失该键的旧配置也落到 30（serde default）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert_eq!(prefs.remote_frame_rate, 30);
        // 往返保留用户选择。
        let saved = toml::to_string(&GuiPrefs {
            remote_frame_rate: 60,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert_eq!(reparsed.remote_frame_rate, 60);
    }

    /// 原生分辨率默认**开启**（4K/2K 桌面不再缩到 1920），且显式关闭后能 TOML 往返保留。
    #[test]
    fn native_resolution_defaults_on_and_survives_roundtrip() {
        assert!(
            GuiPrefs::default().remote_native_resolution,
            "全新安装应默认按原生分辨率编码（缩到 1920 会让文字细节在编码前丢失）"
        );
        // 缺键的旧配置也落到 true（serde default = default_true）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(prefs.remote_native_resolution, "旧配置缺该键应默认开启");
        // 往返保留用户选择（显式关掉）。
        let saved = toml::to_string(&GuiPrefs {
            remote_native_resolution: false,
            remote_native_resolution_migrated: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!reparsed.remote_native_resolution);
    }

    /// 优先 H.265 默认**开启**，缺键的旧配置也落到开启，用户显式关掉后能 TOML 往返保留。
    #[test]
    fn prefer_h265_defaults_on_and_survives_roundtrip() {
        assert!(
            GuiPrefs::default().remote_prefer_h265,
            "全新安装应默认优先 H.265（双方都支持时省 25–50% 码率）"
        );
        // 缺键的旧配置也落到 true（serde default = default_true）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(prefs.remote_prefer_h265, "旧配置缺该键应默认开启");
        // 往返保留用户选择（显式关掉）。
        let saved = toml::to_string(&GuiPrefs {
            remote_prefer_h265: false,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!reparsed.remote_prefer_h265);
    }

    /// 4:4:4（VP9）优先默认**关闭**，缺键的旧配置也落到关闭，用户显式打开后能 TOML 往返保留。
    #[test]
    fn prefer_444_defaults_off_and_survives_roundtrip() {
        assert!(
            !GuiPrefs::default().remote_prefer_444,
            "全新安装应默认关闭（默认走 H.264 硬件编码）"
        );
        // 缺键的旧配置也落到 false（serde default = false）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(!prefs.remote_prefer_444, "旧配置缺该键应默认关闭");
        // 往返保留用户选择（显式打开）。
        let saved = toml::to_string(&GuiPrefs {
            remote_prefer_444: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(reparsed.remote_prefer_444);
    }

    /// 放大锐化默认**开启**（1080p 远端在 4K 全屏是 2× 放大），缺键的旧配置也落到开启，
    /// 用户显式关掉后能 TOML 往返保留。
    #[test]
    fn sharpen_defaults_on_and_survives_roundtrip() {
        assert!(
            GuiPrefs::default().remote_sharpen,
            "全新安装应默认开启放大锐化（否则 2× 放大的文字明显发虚）"
        );
        // 缺键的旧配置也落到 true（serde default = default_true）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(prefs.remote_sharpen, "旧配置缺该键应默认开启");
        // 往返保留用户选择（显式关掉）。
        let saved = toml::to_string(&GuiPrefs {
            remote_sharpen: false,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!reparsed.remote_sharpen, "用户显式关闭必须持久化");
    }

    /// 放大锐化强度：默认 42%、旧配置缺键也是 42、显式 80 能往返；`from_prefs` 把越界值钳到 100。
    #[test]
    fn sharpen_strength_defaults_roundtrips_and_clamps() {
        assert_eq!(
            GuiPrefs::default().remote_sharpen_strength,
            42,
            "全新安装默认强度应 42%"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert_eq!(
            prefs.remote_sharpen_strength, 42,
            "旧配置缺该键也应落到 42%"
        );
        let saved = toml::to_string(&GuiPrefs {
            remote_sharpen_strength: 80,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert_eq!(reparsed.remote_sharpen_strength, 80, "往返必须保留用户选择");

        // `from_prefs` 对越界值（手改 gui.toml）钳到 100。
        let wild = GuiPrefs {
            remote_sharpen_strength: 150,
            ..GuiPrefs::default()
        };
        let view = crate::remote::RemoteView::from_prefs(&wild, None);
        assert_eq!(view.sharpen_strength, 100, "越界强度应在播种时钳到 100");
    }

    /// 放大算法默认 FSR1，旧配置缺键也落到 FSR1，用户选 CAS 后能 TOML 往返保留。
    #[test]
    fn upscale_mode_defaults_fsr_and_roundtrips() {
        assert_eq!(
            GuiPrefs::default().remote_upscale_mode,
            UpscaleMode::Fsr,
            "全新安装默认 FSR1"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert_eq!(prefs.remote_upscale_mode, UpscaleMode::Fsr, "旧配置缺键默认 FSR1");
        let saved = toml::to_string(&GuiPrefs {
            remote_upscale_mode: UpscaleMode::Cas,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert_eq!(reparsed.remote_upscale_mode, UpscaleMode::Cas, "往返保留 CAS");
    }

    /// 原生分辨率迁移只跑一次：旧配置（键为 false、无迁移标志）被改成 true 并落标志，
    /// 之后用户再显式关掉不会被改回去。
    #[test]
    fn native_resolution_migration_runs_once() {
        // 模拟旧配置：显式 false 且没有迁移标志。
        let mut prefs: GuiPrefs = toml::from_str(
            "remote_native_resolution = false\nremote_toolbar_autohide_migrated = true\n",
        )
        .unwrap();
        assert!(!prefs.remote_native_resolution_migrated);
        assert!(migrate_native_resolution(&mut prefs), "首次应发生迁移");
        assert!(prefs.remote_native_resolution, "迁移应改为开启");

        // 用户随后关掉并落盘；再次加载不得被迁移覆盖。
        prefs.remote_native_resolution = false;
        let saved = toml::to_string(&prefs).unwrap();
        let mut reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!migrate_native_resolution(&mut reparsed), "已迁移不得重跑");
        assert!(!reparsed.remote_native_resolution, "用户显式选择必须保留");
    }

    /// 断线自动重连的缺省：**两条路径必须一致**——`Default::default()`（全新安装）与
    /// serde 缺键默认值（旧配置文件）。两处不一致会出现「文档说默认开、实际默认关」。
    #[test]
    fn auto_reconnect_defaults_to_true_in_both_paths() {
        assert!(
            GuiPrefs::default().remote_auto_reconnect,
            "全新安装应默认开启断线自动重连"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(prefs.remote_auto_reconnect, "旧配置缺该键也应默认开启");
        // 用户显式关掉要能持久化。
        let saved = toml::to_string(&GuiPrefs {
            remote_auto_reconnect: false,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!reparsed.remote_auto_reconnect);
    }

    /// 中继→P2P 自动升级的缺省：同样要求两条路径一致（全新安装与旧配置缺键都默认开启），
    /// 否则会出现「文档说会升级、实际永远不升」。
    #[test]
    fn p2p_upgrade_defaults_to_true_in_both_paths() {
        assert!(
            GuiPrefs::default().remote_p2p_upgrade,
            "全新安装应默认开启中继→P2P 自动升级"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(prefs.remote_p2p_upgrade, "旧配置缺该键也应默认开启");
        let saved = toml::to_string(&GuiPrefs {
            remote_p2p_upgrade: false,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(!reparsed.remote_p2p_upgrade, "用户显式关闭必须持久化");
    }

    /// 画质档位的缺省与往返：与帧率同理，旧配置缺键落到 `Balanced`，用户选择跨重启保留。
    #[test]
    fn quality_defaults_to_balanced_and_survives_roundtrip() {
        assert_eq!(
            GuiPrefs::default().remote_quality,
            QualityPreset::Balanced,
            "新配置默认均衡档"
        );
        // 缺失该键的旧配置也落到 Balanced（serde default）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert_eq!(prefs.remote_quality, QualityPreset::Balanced);
        // 往返保留用户选择。
        let saved = toml::to_string(&GuiPrefs {
            remote_quality: QualityPreset::Hd,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert_eq!(reparsed.remote_quality, QualityPreset::Hd);
    }

    /// 工具条收起态的缺省与往返：默认展开，旧配置缺键也是展开，用户收起后跨重启保留。
    #[test]
    fn toolbar_collapsed_defaults_to_expanded_and_survives_roundtrip() {
        assert!(
            !GuiPrefs::default().remote_toolbar_collapsed,
            "新配置默认展开"
        );
        // 缺失该键的旧配置也落到展开（serde default）。
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(!prefs.remote_toolbar_collapsed);
        // 往返保留用户选择。
        let saved = toml::to_string(&GuiPrefs {
            remote_toolbar_collapsed: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(reparsed.remote_toolbar_collapsed);
    }

    /// 自动锁屏的缺省与往返：默认关闭，旧配置缺键也是关闭，用户打开后跨重启保留。
    #[test]
    fn auto_lock_defaults_off_and_survives_roundtrip() {
        assert!(
            !GuiPrefs::default().remote_auto_lock_on_disconnect,
            "新配置默认关闭"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(
            !prefs.remote_auto_lock_on_disconnect,
            "缺键的旧配置也必须落到关闭"
        );
        let saved = toml::to_string(&GuiPrefs {
            remote_auto_lock_on_disconnect: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(
            reparsed.remote_auto_lock_on_disconnect,
            "往返必须保留用户选择"
        );
    }

    /// 统计角标详细档默认关闭，旧配置缺键也落到简洁档，用户开启后跨重启保留。
    #[test]
    fn stats_detail_defaults_off_and_survives_roundtrip() {
        assert!(
            !GuiPrefs::default().remote_stats_detail,
            "新配置默认简洁档（详细档占屏面积大）"
        );
        let prefs: GuiPrefs = toml::from_str("theme = \"system\"\n").unwrap();
        assert!(!prefs.remote_stats_detail, "旧配置缺键也应落到简洁档");
        let saved = toml::to_string(&GuiPrefs {
            remote_stats_detail: true,
            ..GuiPrefs::default()
        })
        .unwrap();
        let reparsed: GuiPrefs = toml::from_str(&saved).unwrap();
        assert!(reparsed.remote_stats_detail, "往返必须保留用户选择");
    }

    #[test]
    fn frame_rate_is_normalized_on_read() {
        assert_eq!(normalize_remote_frame_rate(0), 15, "0 钳到地板");
        assert_eq!(normalize_remote_frame_rate(30), 30);
        assert_eq!(normalize_remote_frame_rate(60), 60);
        assert_eq!(normalize_remote_frame_rate(200), 60, "超上限钳到 60");
    }
}
