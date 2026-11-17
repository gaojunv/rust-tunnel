//! 「映射」页：服务端下发的反向代理规则（规则 → 路径 → 后端）的只读视图。
//!
//! 页面状态（搜索词 / 展开集）由父窗口持有，见 [`MapState`]——本模块只画，不落盘。

use std::collections::HashSet;

use egui::{Align, CornerRadius, FontId, Layout, Pos2, RichText, Sense, Stroke, Ui, Vec2};
use rust_tunnel_client::ClientStatus;
use rust_tunnel_common::protocol::{BackendSummary, MappingSummary, RouteSummary, RuleSummary};

use super::icons::{self, Icon};
use super::theme::palette;
use super::widgets::{self, ButtonKind, Tone};

/// 搜索框宽度（`search_field` 会占满可用宽度，故先套一个固定宽度的容器）。
const SEARCH_WIDTH: f32 = 220.0;

/// 映射页的本地状态（纯视图状态，不落盘）。
#[derive(Default)]
pub struct MapState {
    /// 搜索关键词（空 = 显示全部规则）。
    pub query: String,
    /// 已展开的规则 ID 集合（点 chevron 切换）。
    pub expanded: HashSet<String>,
}

/// 绘制映射页。
pub fn show(ui: &mut Ui, status: &ClientStatus, state: &mut MapState) {
    let Some(summary) = status.mapping_summary.as_ref() else {
        widgets::page_header(ui, "映射", "服务端下发的反向代理规则", |_ui| {});
        widgets::empty_state(
            ui,
            Icon::Route,
            "暂无映射摘要",
            "等待服务端推送，或该客户端未被任何规则引用",
        );
        return;
    };

    widgets::page_header(ui, "映射", "服务端下发的反向代理规则", |ui| {
        // 搜索框固定在右侧；`allocate_ui_with_layout` 给它一个 220 宽的容器。
        ui.allocate_ui_with_layout(
            Vec2::new(SEARCH_WIDTH, 30.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                widgets::search_field(ui, &mut state.query, "搜索规则 / 域名 / 后端");
            },
        );
        // 「全部展开 / 全部折叠」：作用域是当前**可见**（筛选后）的规则，符合所见即所得。
        let ids: Vec<&str> = summary
            .rules
            .iter()
            .filter(|rule| rule_matches(rule, &state.query))
            .map(|rule| rule.id.as_str())
            .collect();
        let all_expanded = !ids.is_empty() && ids.iter().all(|id| state.expanded.contains(*id));
        let (label, icon) = if all_expanded {
            ("全部折叠", Icon::ChevronUp)
        } else {
            ("全部展开", Icon::ChevronDown)
        };
        if widgets::button(ui, label, Some(icon), ButtonKind::Secondary).clicked() {
            for id in &ids {
                if all_expanded {
                    state.expanded.remove(*id);
                } else {
                    state.expanded.insert((*id).to_owned());
                }
            }
        }
    });

    let p = palette(ui.style());
    let matched = summary
        .rules
        .iter()
        .filter(|rule| rule_matches(rule, &state.query))
        .count();
    let query_active = !state.query.trim().is_empty();
    ui.label(
        RichText::new(summary_text(summary, matched, query_active))
            .size(12.0)
            .color(p.text_weak),
    );
    ui.add_space(10.0);

    if summary.truncated {
        widgets::info_bar(
            ui,
            Tone::Warn,
            Some("列表已截断"),
            "规则过多，服务端按 1MB 上限截断下发。",
        );
    }

    if summary.rules.is_empty() {
        widgets::empty_state(ui, Icon::Route, "暂无映射", "该客户端未被任何规则引用");
        return;
    }
    if matched == 0 {
        widgets::empty_state(ui, Icon::Search, "没有匹配的规则", "换个关键词试试");
        return;
    }

    // 规则数量由服务端决定（可能很多），列表必须可滚动，否则窗口下方的卡片会被裁掉。
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for rule in &summary.rules {
                if rule_matches(rule, &state.query) {
                    rule_card(ui, rule, state);
                    ui.add_space(10.0);
                }
            }
        });
}

/// 摘要行：未搜索时给总量，搜索时只给「匹配 / 总数」。
fn summary_text(summary: &MappingSummary, matched: usize, query_active: bool) -> String {
    let total_rules = summary.rules.len();
    if query_active {
        return format!("匹配 {matched} / 总数 {total_rules}");
    }
    let paths: usize = summary.rules.iter().map(|rule| rule.routes.len()).sum();
    let backends: usize = summary
        .rules
        .iter()
        .flat_map(|rule| rule.routes.iter())
        .map(|route| route.backends.len())
        .sum();
    format!("{total_rules} 条规则 · {paths} 条路径 · {backends} 个后端")
}

/// 关键词匹配规则（大小写不敏感；空/纯空白关键词恒匹配）。
///
/// 覆盖规则名、ID、监听地址、域名、路由路径，以及后端的 `kind`/`addr`/`client_name`——
/// 即搜索框提示里的「规则 / 域名 / 路径 / 后端」，另外把 ID 与监听地址也纳入，
/// 便于按服务端告警里的 ID 直接定位。
#[must_use]
fn rule_matches(rule: &RuleSummary, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let hit = |text: &str| text.to_lowercase().contains(&needle);
    if hit(&rule.id) || hit(&rule.name) || hit(&rule.listen) {
        return true;
    }
    if rule.domains.iter().any(|domain| hit(domain)) {
        return true;
    }
    rule.routes.iter().any(|route| {
        hit(&route.path)
            || route
                .backends
                .iter()
                .any(|backend| backend_matches(backend, &needle))
    })
}

/// 后端字段是否命中（`client_name` 可缺省）。
fn backend_matches(backend: &BackendSummary, needle: &str) -> bool {
    let hit = |text: &str| text.to_lowercase().contains(needle);
    hit(&backend.kind) || hit(&backend.addr) || backend.client_name.as_deref().is_some_and(hit)
}

/// 单条规则的卡片：标题（名称 / ID / TLS / chevron）+ 监听 + 域名 +（展开后）路由。
fn rule_card(ui: &mut Ui, rule: &RuleSummary, state: &mut MapState) {
    let p = palette(ui.style());
    let expanded = state.expanded.contains(&rule.id);
    let mut toggle = false;

    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            icons::icon(ui, Icon::Route, 17.0, p.accent);
            ui.label(
                RichText::new(rule.name.as_str())
                    .font(icons::heading_font(16.0))
                    .color(p.text),
            );
            mono_badge(ui, &rule.id, Tone::Neutral);
            widgets::right_row_sized(ui, 30.0, |ui| {
                let chevron = if expanded {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                };
                if widgets::icon_button(ui, chevron, if expanded { "折叠" } else { "展开" })
                    .clicked()
                {
                    toggle = true;
                }
                if rule.tls_enabled {
                    widgets::badge(ui, "TLS 开启", Tone::Ok, Some(Icon::Lock));
                } else {
                    widgets::badge(ui, "TLS 关闭", Tone::Neutral, None);
                }
            });
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("监听").size(12.0).color(p.text_weak));
            ui.label(
                RichText::new(rule.listen.as_str())
                    .monospace()
                    .size(12.5)
                    .color(p.text),
            );
            if widgets::icon_button(ui, Icon::Copy, "复制监听地址").clicked() {
                ui.ctx().copy_text(rule.listen.clone());
            }
        });

        ui.add_space(6.0);
        let (fg, bg) = widgets::tone_colors(&p, Tone::Neutral);
        ui.horizontal_wrapped(|ui| {
            if rule.domains.is_empty() {
                ui.label(RichText::new("无域名").size(12.0).color(p.text_disabled));
            } else {
                for domain in &rule.domains {
                    widgets::pill(ui, domain, fg, bg);
                }
            }
        });

        if expanded {
            ui.add_space(10.0);
            for route in &rule.routes {
                route_section(ui, rule, route);
                ui.add_space(6.0);
            }
        }
    });

    if toggle {
        if expanded {
            state.expanded.remove(&rule.id);
        } else {
            state.expanded.insert(rule.id.clone());
        }
    }
}

/// 一条路由：路径徽标 + 后端数量 + 带竖直引导线的后端列表。
fn route_section(ui: &mut Ui, rule: &RuleSummary, route: &RouteSummary) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        mono_badge(ui, &route.path, Tone::Accent);
        widgets::right_row_sized(ui, 22.0, |ui| {
            ui.label(
                RichText::new(format!("{} 个后端", route.backends.len()))
                    .size(12.0)
                    .color(p.text_weak),
            );
        });
    });
    if route.backends.is_empty() {
        return;
    }

    // 引导线：从路由头下方画到最后一个后端（缩进块内的行缩进 16px，线在内容左侧）。
    let start_y = ui.min_rect().bottom() + 3.0;
    let line_x = ui.cursor().left() + 8.0;
    ui.indent((rule.id.as_str(), route.path.as_str()), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        for backend in &route.backends {
            backend_row(ui, backend);
        }
    });
    let end_y = ui.min_rect().bottom();
    ui.painter().line_segment(
        [Pos2::new(line_x, start_y), Pos2::new(line_x, end_y)],
        Stroke::new(1.0_f32, p.divider),
    );
}

/// 一个后端：kind 徽标 + 等宽地址 +（可选）客户端名 + 权重 + 复制按钮。
fn backend_row(ui: &mut Ui, backend: &BackendSummary) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        let tone = match backend.kind.as_str() {
            "Direct" => Tone::Neutral,
            "Client" => Tone::Ok,
            _ => Tone::Accent,
        };
        widgets::badge(ui, backend.kind.as_str(), tone, None);
        ui.label(
            RichText::new(backend.addr.as_str())
                .monospace()
                .size(12.5)
                .color(p.text),
        );
        if let Some(name) = backend.client_name.as_deref() {
            let (fg, bg) = widgets::tone_colors(&p, Tone::Neutral);
            widgets::pill(ui, name, fg, bg);
        }
        widgets::right_row_sized(ui, 24.0, |ui| {
            if widgets::icon_button(ui, Icon::Copy, "复制后端地址").clicked() {
                ui.ctx().copy_text(backend.addr.clone());
            }
            ui.label(
                RichText::new(format!("权重 {}", backend.weight))
                    .size(12.0)
                    .color(p.text_weak),
            );
        });
    });
}

/// 等宽字体徽标（规则 ID / 路由路径）：形状与 [`widgets::badge`] 一致，字体换成等宽。
fn mono_badge(ui: &mut Ui, text: &str, tone: Tone) {
    let p = palette(ui.style());
    let (fg, bg) = widgets::tone_colors(&p, tone);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::monospace(11.5), fg);
    let pad = Vec2::new(7.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
    ui.painter().galley(rect.min + pad, galley, fg);
}

#[cfg(test)]
mod tests {
    use super::rule_matches;
    use rust_tunnel_common::protocol::{BackendSummary, RouteSummary, RuleSummary};

    fn sample() -> RuleSummary {
        RuleSummary {
            id: "rule-7".to_owned(),
            name: "Web API".to_owned(),
            listen: "0.0.0.0:8443".to_owned(),
            domains: vec!["api.example.com".to_owned(), "www.example.com".to_owned()],
            tls_enabled: true,
            routes: vec![
                RouteSummary {
                    path: "/v1".to_owned(),
                    backends: vec![BackendSummary {
                        kind: "Client".to_owned(),
                        addr: "10.0.0.5:8080".to_owned(),
                        client_name: Some("home-nas".to_owned()),
                        weight: 2,
                    }],
                },
                RouteSummary {
                    path: "/static".to_owned(),
                    backends: vec![BackendSummary {
                        kind: "Direct".to_owned(),
                        addr: "127.0.0.1:9000".to_owned(),
                        client_name: None,
                        weight: 1,
                    }],
                },
            ],
        }
    }

    #[test]
    fn empty_query_matches_every_rule() {
        let rule = sample();
        assert!(rule_matches(&rule, ""));
        assert!(rule_matches(&rule, "   "), "纯空白关键词应视为未搜索");
    }

    #[test]
    fn matches_rule_name_case_insensitively() {
        let rule = sample();
        assert!(rule_matches(&rule, "web api"));
        assert!(rule_matches(&rule, "WEB"));
        assert!(rule_matches(&rule, "  ApI  "), "查询首尾空白应被裁掉");
    }

    #[test]
    fn matches_id_and_listen() {
        let rule = sample();
        assert!(rule_matches(&rule, "rule-7"));
        assert!(rule_matches(&rule, "0.0.0.0:8443"));
    }

    #[test]
    fn matches_domains() {
        let rule = sample();
        assert!(rule_matches(&rule, "example.com"));
        assert!(rule_matches(&rule, "API.EXAMPLE"), "大写查询应命中小写域名");
        assert!(rule_matches(&rule, "www."));
    }

    #[test]
    fn matches_route_paths() {
        let rule = sample();
        assert!(rule_matches(&rule, "/V1"), "大写查询应命中小写路径");
        assert!(rule_matches(&rule, "stat"));
    }

    #[test]
    fn matches_backend_addr_client_and_kind() {
        let rule = sample();
        assert!(rule_matches(&rule, "10.0.0.5"));
        assert!(rule_matches(&rule, "HOME-NAS"));
        assert!(rule_matches(&rule, "127.0.0.1:9000"));
        assert!(rule_matches(&rule, "client"), "kind 大小写不敏感");
        assert!(rule_matches(&rule, "Direct"));
    }

    #[test]
    fn non_matching_query_is_rejected() {
        let rule = sample();
        assert!(!rule_matches(&rule, "nope"));
        assert!(!rule_matches(&rule, "https://"));
    }
}
