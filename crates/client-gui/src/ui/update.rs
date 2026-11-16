//! 客户端自更新的 UI：更新弹窗与设置页「软件更新」一节。
//!
//! 弹窗与远程接入确认同理，**常驻所有 Tab 之上**：窗口可能被隐藏到托盘，更新提示
//! 不能只在某一页可见；`GuiApp::update` 在首次出现时会把窗口叫回来。

use egui::{RichText, Ui};

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

    let mut action = None;
    egui::Modal::new(egui::Id::new("client_update")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading("客户端更新");
        ui.add_space(6.0);

        if let Some(progress) = snap.download.as_ref() {
            ui.label(RichText::new(format!("正在下载 {}", progress.version)).strong());
            ui.add_space(4.0);
            let fraction = fraction(progress.received, progress.total);
            ui.add(
                egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                    .desired_width(340.0)
                    .text(format!(
                        "{} / {}",
                        human_bytes(progress.received),
                        human_bytes(progress.total)
                    )),
            );
            ui.label(
                RichText::new("下载完成后会自动校验并重启应用。")
                    .weak()
                    .small(),
            );
            return;
        }

        if let Some(err) = snap.error.as_ref() {
            ui.colored_label(ui.visuals().error_fg_color, RichText::new(err).strong());
            ui.add_space(6.0);
            if snap.available.is_some() && ui.button("重试").clicked() {
                action = Some(UpdateAction::Download);
            }
            ui.horizontal(|ui| {
                if ui.button("关闭").clicked() {
                    action = Some(UpdateAction::ClearError);
                }
            });
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
            .small()
            .weak(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(RichText::new("立即更新").strong()))
                .clicked()
            {
                action = Some(UpdateAction::Download);
            }
            if ui.button("稍后").clicked() {
                action = Some(UpdateAction::Dismiss);
            }
            if ui.button("跳过此版本").clicked() {
                action = Some(UpdateAction::Skip(available.version.clone()));
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("更新通过已加密的控制通道下载，并校验 SHA-256 后才替换本机程序。")
                .weak()
                .small(),
        );
    });
    action
}

/// 设置页的「软件更新」一节。
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

    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.heading("软件更新");
            ui.add_space(4.0);
            egui::Grid::new("update_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("当前版本").weak().small());
                    ui.label(env!("CARGO_PKG_VERSION"));
                    ui.end_row();
                    if let Some(version) = snap.up_to_date.as_ref() {
                        ui.label(RichText::new("最新版本").weak().small());
                        ui.label(version.as_str());
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            if ui
                .checkbox(&mut prefs.update_auto_check, "自动检查更新")
                .changed()
            {
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
                ui.add(
                    egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                        .desired_width(320.0)
                        .text(format!(
                            "{} / {}",
                            human_bytes(progress.received),
                            human_bytes(progress.total)
                        )),
                );
                return;
            }

            if let Some(err) = snap.error.as_ref() {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, RichText::new(err).small());
                if ui.button("清除").clicked() {
                    action = Some(UpdateAction::ClearError);
                }
                return;
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let supported = snap.supported;
                if ui
                    .add_enabled(supported, egui::Button::new("检查更新"))
                    .clicked()
                {
                    action = Some(UpdateAction::CheckNow);
                }
                if !supported {
                    ui.label(
                        RichText::new("连接服务器后可用；Windows 服务接管身份期间不可用")
                            .weak()
                            .small(),
                    );
                }
            });

            if let Some(available) = snap.available.as_ref() {
                ui.add_space(4.0);
                let skipped =
                    prefs.update_skipped_version.as_deref() == Some(available.version.as_str());
                ui.horizontal(|ui| {
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
                });
                ui.horizontal(|ui| {
                    if skipped {
                        if ui.button("取消跳过").clicked() {
                            action = Some(UpdateAction::CancelSkip);
                        }
                    } else {
                        if ui
                            .add(egui::Button::new(RichText::new("立即更新").strong()))
                            .clicked()
                        {
                            action = Some(UpdateAction::Download);
                        }
                        if ui.button("跳过此版本").clicked() {
                            action = Some(UpdateAction::Skip(available.version.clone()));
                        }
                    }
                });
            } else if let Some(version) = snap.up_to_date.as_ref() {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("已是最新版本（{version}）")).weak());
            }
        });
    (changed, action)
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
