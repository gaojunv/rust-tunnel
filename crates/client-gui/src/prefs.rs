//! GUI 专属偏好持久化（gui.toml，与 client 配置同目录、互不干扰）。

use std::path::PathBuf;

use rust_tunnel_remote::ApprovalPolicy;

use crate::config_store;
use crate::remote::{ViewScale, WindowSize};

/// 布尔字段的 `serde(default = ...)` 目标：缺省为 true。
///
/// 不能对「默认应为 true」的 bool 直接用裸 `#[serde(default)]`——`bool::default()` 是 false，
/// 旧配置文件缺字段时会静默落到 false（对「自动重连」「工具条自动隐藏」这类默认开启的行为是
/// 反直觉的）。
fn default_true() -> bool {
    true
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

    /// 按钮图标。
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::System => "💻",
            Self::Light => "☀",
            Self::Dark => "🌙",
        }
    }
}

/// GUI 偏好根结构（serde(default) 保证前向兼容）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// 远程会话窗口：工具条是否自动隐藏（指针静止后淡出，移到上缘或移动指针再现）。
    #[serde(default = "default_true")]
    pub remote_toolbar_autohide: bool,
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
    /// 会话窗口内的统计角标（延迟 / 路径 / 分辨率 / 帧率 / 时长）是否常显。
    #[serde(default)]
    pub remote_stats_overlay: bool,
}

/// GUI 偏好文件路径（与 client 配置同目录的 gui.toml）。
#[must_use]
pub fn path() -> PathBuf {
    config_store::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("gui.toml"), |dir| dir.join("gui.toml"))
}

/// 加载 GUI 偏好；文件缺失/读取/解析失败时返回默认值。
#[must_use]
pub fn load() -> GuiPrefs {
    let p = path();
    let Ok(content) = std::fs::read_to_string(&p) else {
        return GuiPrefs::default();
    };
    match toml::from_str::<GuiPrefs>(&content) {
        Ok(prefs) => prefs,
        Err(e) => {
            tracing::warn!("GUI 偏好解析失败（{}），使用默认值：{e}", p.display());
            GuiPrefs::default()
        }
    }
}

/// 原子写入 GUI 偏好（临时文件 + rename，避免崩溃写半截）。
///
/// # Errors
/// 当序列化或文件写入失败时返回 `Err`。
pub fn save(prefs: &GuiPrefs) -> Result<(), config_store::StoreError> {
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
