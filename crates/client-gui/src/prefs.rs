//! GUI 专属偏好持久化（gui.toml，与 client 配置同目录、互不干扰）。

use std::path::PathBuf;

use crate::config_store;

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
pub struct GuiPrefs {
    /// 主题偏好。
    #[serde(default)]
    pub theme: ThemePref,
    /// 是否允许被其他客户端远程控制。**默认关闭**：开启等于把本机屏幕与输入
    /// 交给持有效 token 的对端，必须是用户的显式选择，且随进程重启保留。
    #[serde(default)]
    pub allow_remote: bool,
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
