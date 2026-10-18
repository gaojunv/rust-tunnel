//! GUI 专属偏好持久化（gui.toml，与 client 配置同目录、互不干扰）。

use std::path::PathBuf;

use rust_tunnel_remote::ApprovalPolicy;

use crate::config_store;
use crate::remote::{ViewScale, WindowSize};

/// 布尔字段的 `serde(default = ...)` 目标：缺省为 true。
///
/// 不能对「默认应为 true」的 bool 直接用裸 `#[serde(default)]`——`bool::default()` 是 false，
/// 旧配置文件缺字段时会静默落到 false（对「自动重连」这类默认开启的行为是反直觉的）。
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
    ///
    /// **默认关闭**：工具条是会话里唯一的操作入口，默认常驻最省心；要淡出的人可开。
    /// 注意 `save()` 会序列化每个字段，所以**现有用户的 `gui.toml` 早已把旧默认 `true` 钉死**，
    /// 改这里的默认值只对**全新配置**生效——真正让老用户能关掉它的是工具条上的「自动隐藏」开关
    /// （见 `ui::session_window::toolbar`），不是这个默认值。
    #[serde(default)]
    pub remote_toolbar_autohide: bool,
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

/// 加载 GUI 偏好；文件缺失/读取/解析失败时返回默认值。
#[must_use]
pub fn load() -> GuiPrefs {
    let p = path();
    let Ok(content) = std::fs::read_to_string(&p) else {
        // 没有配置文件 = 全新安装：没有任何旧值需要迁移，直接把标志置位。否则「全新安装 →
        // 用户打开自动隐藏（落盘 autohide=true, migrated=false）→ 重启」会被迁移误伤改回去。
        return GuiPrefs {
            remote_toolbar_autohide_migrated: true,
            ..GuiPrefs::default()
        };
    };
    match toml::from_str::<GuiPrefs>(&content) {
        Ok(mut prefs) => {
            // 迁移必须立刻落盘，否则标志在磁盘上仍是 false，每次启动都会重跑。
            if migrate_toolbar_autohide(&mut prefs) {
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
                ..GuiPrefs::default()
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧配置（未迁移 + 旧默认 true）→ 迁移后标志置位、自动隐藏强制关闭。
    #[test]
    fn old_config_gets_migrated() {
        let mut prefs = GuiPrefs {
            remote_toolbar_autohide: true,
            ..GuiPrefs::default()
        };
        assert!(!prefs.remote_toolbar_autohide_migrated, "构造的应是未迁移态");

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
        assert!(prefs.remote_toolbar_autohide, "用户重新打开的自动隐藏必须保留");
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
            .filter(|line| !line.trim_start().starts_with("remote_toolbar_autohide_migrated"))
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
        assert!(reparsed.remote_toolbar_autohide_migrated, "迁移标志应已落盘");
        assert!(
            !migrate_toolbar_autohide(&mut reparsed),
            "已迁移的文件不得再次迁移"
        );
        assert!(!reparsed.remote_toolbar_autohide);
    }
}
