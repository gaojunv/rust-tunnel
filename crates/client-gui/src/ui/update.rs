//! 客户端自更新的 UI：更新弹窗与设置页「软件更新」一节。
//!
//! 弹窗与远程接入确认同理，**常驻所有 Tab 之上**：窗口可能被隐藏到托盘，更新提示
//! 不能只在某一页可见；`GuiApp::update` 在首次出现时会把窗口叫回来。

use egui::{RichText, Ui};

use super::theme::palette;
use super::widgets::{self, Tone};
use crate::prefs::GuiPrefs;
use crate::update::UpdateBridge;

/// 用户在更新 UI 上触发的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    /// 开始下载并安装。
    Download,
    /// 稍后再提醒（隐藏弹窗，保留可用版本）。
    Dismiss,
    /// 跳过此版本（落盘偏好）。
    Skip(String),
    /// 取消「跳过此版本」。
    CancelSkip,
    /// 立即检查更新。
    CheckNow,
    /// 清除错误。
    ClearError,
}

/// 更新提示弹窗（每帧由 `GuiApp::update` 调用）。
///
/// 返回 `Some(action)` 表示用户点了按钮；弹窗的显示条件：有可用更新且未被「稍后」
/// 隐藏且未被「跳过此版本」，或者正在下载/最近有错误（错误必须让用户看到）。
#[must_use]
pub fn modal(
    ctx: &egui::Context,
    update: &UpdateBridge,
    skipped: Option<&str>,
) -> Option<UpdateAction> {
    let snap = update.snapshot();
    let available_visible = snap
        .available
        .as_ref()
        .is_some_and(|a| !snap.dismissed && Some(a.version.as_str()) != skipped);
    let visible = available_visible || snap.download.is_some() || snap.error.is_some();
    if !visible {
        return None;
    }

    let p = palette(&ctx.style());
    let mut action = None;
    egui::Modal::new(egui::Id::new("client_update")).show(ctx, |ui| {
        ui.set_width(380.0);
        let close = widgets::modal_title(ui, "客户端更新");
        ui.add_space(4.0);

        if let Some(progress) = snap.download.as_ref() {
            ui.label(RichText::new(format!("正在下载 {}", progress.version)).strong());
            ui.add_space(4.0);
            let fraction = fraction(progress.received, progress.total);
            progress_bar(
                ui,
                fraction,
                &format!(
                    "{} / {}",
                    human_bytes(progress.received),
                    human_bytes(progress.total)
                ),
            );
            ui.label(
                RichText::new("下载完成后会自动校验并重启应用。")
                    .size(12.0)
                    .color(p.text_weak),
            );
            // 下载期间弹窗由 `download` 保持可见（进度必须能被看到），「关闭」与「稍后」同语义。
            if close {
                action = Some(UpdateAction::Dismiss);
            }
            return;
        }

        if let Some(err) = snap.error.as_ref() {
            widgets::info_bar(ui, Tone::Err, None, err);
            ui.horizontal(|ui| {
                if snap.available.is_some() && widgets::primary_button(ui, "重试").clicked() {
                    action = Some(UpdateAction::Download);
                }
                if widgets::secondary_button(ui, "关闭").clicked() {
                    action = Some(UpdateAction::ClearError);
                }
            });
            if close {
                action = Some(UpdateAction::ClearError);
            }
            return;
        }

        let Some(available) = snap.available.as_ref() else {
            return;
        };
        ui.label(
            RichText::new(format!(
                "发现新版本 {}（当前 {}）",
                available.version,
                env!("CARGO_PKG_VERSION")
            ))
            .strong(),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "安装包 {} · {}。下载并校验后应用会自动重启。",
                human_bytes(available.file.size),
                available.file.name
            ))
            .size(12.0)
            .color(p.text_weak),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, "立即更新").clicked() {
                action = Some(UpdateAction::Download);
            }
            if widgets::secondary_button(ui, "稍后").clicked() {
                action = Some(UpdateAction::Dismiss);
            }
            if widgets::secondary_button(ui, "跳过此版本").clicked() {
                action = Some(UpdateAction::Skip(available.version.clone()));
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("更新通过已加密的控制通道下载，并校验 SHA-256 后才替换本机程序。")
                .size(12.0)
                .color(p.text_weak),
        );
        if close {
            action = Some(UpdateAction::Dismiss);
        }
    });
    action
}

/// 设置页的「软件更新」一节（Fluent 卡片）。
///
/// 返回 `(偏好是否变化, 用户动作)`；调用方负责落盘偏好并执行动作。
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "更新设置的单屏表单：状态分支（下载中/错误/可用/已最新）与按钮组必须放在一起"
)]
pub fn settings_section(
    ui: &mut Ui,
    update: &UpdateBridge,
    prefs: &mut GuiPrefs,
) -> (bool, Option<UpdateAction>) {
    let mut action = None;
    let mut changed = false;
    let snap = update.snapshot();
    let p = palette(ui.style());

    widgets::card_titled(
        ui,
        "软件更新",
        Some("通过已加密的控制通道下载，并校验 SHA-256 后替换本机程序"),
        |ui| {
            widgets::key_value(ui, "当前版本", env!("CARGO_PKG_VERSION"), false);
            if let Some(version) = snap.up_to_date.as_ref() {
                widgets::key_value(ui, "最新版本", version.as_str(), false);
            }
            ui.add_space(4.0);
            if widgets::switch_row(
                ui,
                "自动检查更新",
                Some("每 6 小时轮询一次；服务端在注册时也会主动推送"),
                &mut prefs.update_auto_check,
            ) {
                changed = true;
                tracing::info!(
                    "自动检查更新：{}",
                    if prefs.update_auto_check {
                        "开"
                    } else {
                        "关"
                    }
                );
            }

            if let Some(progress) = snap.download.as_ref() {
                ui.add_space(4.0);
                ui.label(format!("正在下载 {}…", progress.version));
                let fraction = fraction(progress.received, progress.total);
                progress_bar(
                    ui,
                    fraction,
                    &format!(
                        "{} / {}",
                        human_bytes(progress.received),
                        human_bytes(progress.total)
                    ),
                );
                return;
            }

            if let Some(err) = snap.error.as_ref() {
                ui.add_space(4.0);
                widgets::info_bar(ui, Tone::Err, None, err);
                if widgets::secondary_button(ui, "清除").clicked() {
                    action = Some(UpdateAction::ClearError);
                }
                return;
            }

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let supported = snap.supported;
                let check = ui
                    .add_enabled_ui(supported, |ui| {
                        if !supported {
                            ui.set_opacity(0.45);
                        }
                        widgets::secondary_button(ui, "检查更新")
                    })
                    .inner;
                if check.clicked() {
                    action = Some(UpdateAction::CheckNow);
                }
                if !supported {
                    ui.label(
                        RichText::new("连接服务器后可用；Windows 服务接管身份期间不可用")
                            .size(12.0)
                            .color(p.text_weak),
                    );
                }
            });

            if let Some(available) = snap.available.as_ref() {
                ui.add_space(6.0);
                let skipped =
                    prefs.update_skipped_version.as_deref() == Some(available.version.as_str());
                widgets::card(ui, |ui| {
                    ui.label(
                        RichText::new(if skipped {
                            format!("新版本 {} 已跳过", available.version)
                        } else {
                            format!(
                                "发现新版本 {}（{}）",
                                available.version,
                                human_bytes(available.file.size)
                            )
                        })
                        .strong(),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if skipped {
                            if widgets::secondary_button(ui, "取消跳过").clicked() {
                                action = Some(UpdateAction::CancelSkip);
                            }
                        } else {
                            if widgets::primary_button(ui, "立即更新").clicked() {
                                action = Some(UpdateAction::Download);
                            }
                            if widgets::secondary_button(ui, "跳过此版本").clicked() {
                                action = Some(UpdateAction::Skip(available.version.clone()));
                            }
                        }
                    });
                });
            } else if let Some(version) = snap.up_to_date.as_ref() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("已是最新版本（{version}）"))
                        .size(12.0)
                        .color(p.text_weak),
                );
            }
        },
    );
    (changed, action)
}

/// Fluent 风格的下载进度条：轨道 + 强调色填充 + 下方一行字节数。
///
/// `egui::ProgressBar` 用的是全局 `widgets` 配色，与 Fluent 的圆角/强调色不一致，故自绘。
fn progress_bar(ui: &mut Ui, fraction: f32, text: &str) {
    let p = palette(ui.style());
    let width = ui.available_width().max(140.0);
    let (track, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::hover());
    let radius = egui::CornerRadius::same(4);
    ui.painter().rect_filled(track, radius, p.surface_alt);
    let filled = egui::Rect::from_min_size(
        track.min,
        egui::vec2(track.width() * fraction.clamp(0.0, 1.0), track.height()),
    );
    if filled.width() > 0.5 {
        ui.painter().rect_filled(filled, radius, p.accent);
    }
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(12.0).color(p.text_weak));
}

/// 下载进度比例（0.0–1.0）。
#[allow(
    clippy::cast_precision_loss,
    reason = "文件大小远小于 f32 精度上限，进度条只做展示"
)]
fn fraction(received: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (received as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// 人类可读的字节数（更新包通常是几 MB 到几十 MB）。
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    #[allow(
        clippy::cast_precision_loss,
        reason = "文件大小远小于 f64 精度上限，展示用途"
    )]
    let n = bytes as f64;
    if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
