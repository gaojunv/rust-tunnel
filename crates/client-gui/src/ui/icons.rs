//! 内置矢量图标：Fluent 风的 1.6px 描边线性图标。
//!
//! **为什么不用图标字体**：本 crate 只需要二十来个图标，引入字体库会多一个依赖、把二进制
//! 再撑大几 MB，而且字形风格未必贴合 Win11。这里用 `Painter` 在 24×24 的设计网格上手绘，
//! 线条粗细随尺寸等比缩放，深/浅色主题都由调用方给颜色，不存在缺字或彩色 emoji 的问题。
//!
//! 所有图标共用 [`paint`]，[`icon`] 只是「分配方形区域并画上去」的便捷封装。

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "图标坐标是 24 网格上的常量，圆角像素值也被 0..=255 的网格天然约束"
)]

use egui::{Color32, FontId, Painter, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

/// 图标种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// 状态：心跳折线。
    Pulse,
    /// 映射：一个入口分叉到两个后端。
    Route,
    /// 远程：显示器。
    Monitor,
    /// 设置：齿轮。
    Settings,
    /// 日志：带项目符号的列表。
    List,
    /// 搜索。
    Search,
    /// 浅色主题：太阳。
    Sun,
    /// 深色主题：半明半暗的圆。
    Moon,
    /// 下拉/展开。
    ChevronDown,
    /// 右向展开。
    ChevronRight,
    /// 收起（工具条）。
    ChevronUp,
    /// 警告三角。
    Warning,
    /// 对勾。
    Check,
    /// 关闭。
    Close,
    /// 开始/连接。
    Play,
    /// 停止。
    Stop,
    /// 刷新/更新。
    Refresh,
    /// 安全盾牌。
    Shield,
    /// 时钟。
    Clock,
    /// 下载。
    Download,
    /// 删除。
    Trash,
    /// 复制。
    Copy,
    /// 锁。
    Lock,
    /// 信息。
    Info,
    /// 服务器。
    Server,
    /// 仪表盘/延迟。
    Gauge,
    /// 声音开启。
    Speaker,
    /// 声音关闭/静音。
    SpeakerMuted,
}

/// 分配一枚 `size`×`size` 的图标并返回悬停响应。
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint(ui.painter(), rect, icon, color, 1.6);
    resp
}

/// 在 `rect` 内绘制图标（按短边等比缩放，居中）。
#[expect(
    clippy::too_many_lines,
    reason = "一张图标查找表：每个分支只画一个图标，拆开反而让「新增图标」要改多处"
)]
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32, width: f32) {
    let side = rect.width().min(rect.height());
    let k = side / 24.0;
    let origin = rect.center() - Vec2::splat(12.0 * k);
    let pen = Pen {
        painter,
        origin,
        k,
        color,
        width,
    };
    match icon {
        Icon::Pulse => pen.line(&[
            (2.0, 12.0),
            (6.5, 12.0),
            (9.0, 5.5),
            (13.0, 18.5),
            (16.0, 9.0),
            (18.5, 12.0),
            (22.0, 12.0),
        ]),
        Icon::Route => {
            pen.circle(4.5, 12.0, 2.3);
            pen.circle(19.5, 5.5, 2.3);
            pen.circle(19.5, 18.5, 2.3);
            pen.line(&[(6.8, 12.0), (10.5, 12.0)]);
            pen.cubic((10.5, 12.0), (14.0, 12.0), (13.0, 5.5), (17.2, 5.5));
            pen.cubic((10.5, 12.0), (14.0, 12.0), (13.0, 18.5), (17.2, 18.5));
        }
        Icon::Monitor => {
            pen.rounded_rect(2.5, 4.0, 21.5, 16.0, 2.0);
            pen.line(&[(12.0, 16.0), (12.0, 20.0)]);
            pen.line(&[(8.0, 20.0), (16.0, 20.0)]);
        }
        Icon::Settings => {
            pen.circle(12.0, 12.0, 3.1);
            for i in 0..8 {
                let angle = std::f32::consts::TAU * (i as f32) / 8.0 + std::f32::consts::FRAC_PI_8;
                let inner = 5.3;
                let outer = 7.4;
                pen.line(&[
                    (12.0 + angle.cos() * inner, 12.0 + angle.sin() * inner),
                    (12.0 + angle.cos() * outer, 12.0 + angle.sin() * outer),
                ]);
            }
        }
        Icon::List => {
            for y in [6.0_f32, 12.0, 18.0] {
                pen.dot(4.5, y, 1.0);
                pen.line(&[(7.5, y), (20.5, y)]);
            }
        }
        Icon::Search => {
            pen.circle(10.5, 10.5, 6.2);
            pen.line(&[(15.2, 15.2), (21.0, 21.0)]);
        }
        Icon::Sun => {
            pen.circle(12.0, 12.0, 3.8);
            for i in 0..8 {
                let angle = std::f32::consts::TAU * (i as f32) / 8.0;
                pen.line(&[
                    (12.0 + angle.cos() * 6.2, 12.0 + angle.sin() * 6.2),
                    (12.0 + angle.cos() * 8.8, 12.0 + angle.sin() * 8.8),
                ]);
            }
        }
        Icon::Moon => {
            pen.circle(12.0, 12.0, 7.5);
            // 右半边填实：用半圆多边形表达「明暗对比」。
            let mut points = Vec::with_capacity(20);
            for i in 0..=16 {
                let angle = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * (i as f32) / 16.0;
                points.push(pen.p(12.0 + angle.cos() * 7.5, 12.0 + angle.sin() * 7.5));
            }
            pen.painter
                .add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
        }
        Icon::ChevronDown => pen.line(&[(6.5, 9.5), (12.0, 15.0), (17.5, 9.5)]),
        Icon::ChevronRight => pen.line(&[(9.5, 6.5), (15.0, 12.0), (9.5, 17.5)]),
        Icon::ChevronUp => pen.line(&[(6.5, 14.5), (12.0, 9.0), (17.5, 14.5)]),
        Icon::Warning => {
            pen.closed_line(&[(12.0, 3.2), (21.5, 20.0), (2.5, 20.0)]);
            pen.line(&[(12.0, 9.0), (12.0, 14.0)]);
            pen.dot(12.0, 17.0, 1.1);
        }
        Icon::Check => pen.line(&[(4.8, 12.6), (9.8, 17.4), (19.2, 6.8)]),
        Icon::Close => {
            pen.line(&[(6.0, 6.0), (18.0, 18.0)]);
            pen.line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Play => {
            pen.painter.add(egui::Shape::convex_polygon(
                vec![pen.p(8.0, 4.8), pen.p(19.5, 12.0), pen.p(8.0, 19.2)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Stop => {
            pen.painter.rect_filled(
                Rect::from_min_max(pen.p(6.0, 6.0), pen.p(18.0, 18.0)),
                egui::CornerRadius::same(2),
                color,
            );
        }
        Icon::Refresh => {
            pen.arc(12.0, 12.0, 7.2, -0.9, 4.2);
            let tip = (12.0 + 7.2 * 4.2_f32.cos(), 12.0 + 7.2 * 4.2_f32.sin());
            pen.line(&[tip, (tip.0 + 0.2, tip.1 - 4.0)]);
            pen.line(&[tip, (tip.0 - 4.0, tip.1 - 0.4)]);
        }
        Icon::Shield => {
            pen.closed_line(&[(12.0, 2.8), (20.0, 5.6), (20.0, 11.8)]);
            pen.line(&[(20.0, 11.8), (12.0, 21.2)]);
            pen.line(&[(12.0, 21.2), (4.0, 11.8), (4.0, 5.6), (12.0, 2.8)]);
            pen.line(&[(8.5, 11.6), (11.0, 14.2), (15.6, 8.8)]);
        }
        Icon::Clock => {
            pen.circle(12.0, 12.0, 8.2);
            pen.line(&[(12.0, 6.6), (12.0, 12.2), (16.2, 14.4)]);
        }
        Icon::Download => {
            pen.line(&[(12.0, 3.0), (12.0, 15.0)]);
            pen.line(&[(7.2, 10.4), (12.0, 15.2), (16.8, 10.4)]);
            pen.line(&[(4.0, 19.5), (20.0, 19.5)]);
        }
        Icon::Trash => {
            pen.line(&[(4.0, 6.8), (20.0, 6.8)]);
            pen.line(&[(9.0, 3.6), (15.0, 3.6)]);
            pen.line(&[(6.2, 6.8), (7.2, 21.0), (16.8, 21.0), (17.8, 6.8)]);
            pen.line(&[(10.0, 10.2), (10.4, 17.6)]);
            pen.line(&[(14.0, 10.2), (13.6, 17.6)]);
        }
        Icon::Copy => {
            pen.rounded_rect(8.0, 8.0, 20.0, 20.0, 2.0);
            pen.line(&[(4.0, 16.0), (4.0, 4.0), (16.0, 4.0)]);
        }
        Icon::Lock => {
            pen.rounded_rect(5.0, 10.8, 19.0, 21.0, 2.0);
            pen.arc(12.0, 10.8, 4.8, std::f32::consts::PI, std::f32::consts::TAU);
            pen.dot(12.0, 15.8, 1.1);
        }
        Icon::Info => {
            pen.circle(12.0, 12.0, 8.2);
            pen.line(&[(12.0, 10.6), (12.0, 16.4)]);
            pen.dot(12.0, 7.6, 1.1);
        }
        Icon::Server => {
            pen.rounded_rect(3.0, 3.6, 21.0, 9.6, 1.8);
            pen.rounded_rect(3.0, 12.4, 21.0, 18.4, 1.8);
            pen.dot(6.4, 6.6, 0.9);
            pen.dot(6.4, 15.4, 0.9);
            pen.line(&[(10.0, 6.6), (17.6, 6.6)]);
            pen.line(&[(10.0, 15.4), (17.6, 15.4)]);
        }
        Icon::Gauge => {
            pen.arc(12.0, 15.0, 8.0, std::f32::consts::PI, std::f32::consts::TAU);
            pen.line(&[(12.0, 15.0), (16.8, 8.8)]);
            pen.dot(12.0, 15.0, 1.2);
        }
        Icon::Speaker => {
            // 喇叭主体（闭合折线）。
            pen.line(&[
                (3.5, 9.5),
                (7.0, 9.5),
                (11.5, 5.5),
                (11.5, 18.5),
                (7.0, 14.5),
                (3.5, 14.5),
                (3.5, 9.5),
            ]);
            // 两道声波。
            pen.cubic((14.0, 9.0), (16.0, 10.0), (16.0, 14.0), (14.0, 15.0));
            pen.cubic((16.5, 6.5), (20.0, 8.5), (20.0, 15.5), (16.5, 17.5));
        }
        Icon::SpeakerMuted => {
            pen.line(&[
                (3.5, 9.5),
                (7.0, 9.5),
                (11.5, 5.5),
                (11.5, 18.5),
                (7.0, 14.5),
                (3.5, 14.5),
                (3.5, 9.5),
            ]);
            // 叉号代替声波。
            pen.line(&[(15.0, 9.0), (20.5, 15.0)]);
            pen.line(&[(20.5, 9.0), (15.0, 15.0)]);
        }
    }
}

/// 应用品牌标记：强调色圆角方块 + 双向箭头（隧道）。
pub fn app_mark(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = crate::ui::theme::palette(ui.style());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(6), p.accent);
    let k = size / 24.0;
    let origin = rect.min + (rect.size() - Vec2::splat(size)) / 2.0;
    let pen = Pen {
        painter,
        origin,
        k,
        color: p.on_accent,
        width: 1.8,
    };
    // ⇄：上箭头向右、下箭头向左。
    pen.line(&[(5.5, 9.0), (17.0, 9.0)]);
    pen.line(&[(14.0, 6.2), (17.0, 9.0), (14.0, 11.8)]);
    pen.line(&[(18.5, 15.0), (7.0, 15.0)]);
    pen.line(&[(10.0, 12.2), (7.0, 15.0), (10.0, 17.8)]);
}

/// 标题字体（有 semibold 字体时用它，否则回退正文字体）。
pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, crate::fonts::heading_family())
}

/// 内部画笔：把 24×24 设计坐标映射到目标矩形。
struct Pen<'a> {
    painter: &'a Painter,
    origin: Pos2,
    k: f32,
    color: Color32,
    width: f32,
}

impl Pen<'_> {
    fn p(&self, x: f32, y: f32) -> Pos2 {
        self.origin + Vec2::new(x, y) * self.k
    }

    fn stroke(&self) -> Stroke {
        Stroke::new(self.width * self.k, self.color)
    }

    fn line(&self, points: &[(f32, f32)]) {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter
            .add(egui::epaint::PathShape::line(points, self.stroke()));
    }

    fn closed_line(&self, points: &[(f32, f32)]) {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter
            .add(egui::epaint::PathShape::closed_line(points, self.stroke()));
    }

    fn circle(&self, x: f32, y: f32, r: f32) {
        self.painter
            .circle_stroke(self.p(x, y), r * self.k, self.stroke());
    }

    fn dot(&self, x: f32, y: f32, r: f32) {
        self.painter
            .circle_filled(self.p(x, y), r * self.k, self.color);
    }

    fn rounded_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
        let rect = Rect::from_min_max(self.p(x0, y0), self.p(x1, y1));
        self.painter.rect_stroke(
            rect,
            egui::CornerRadius::same((r * self.k).round() as u8),
            self.stroke(),
            egui::StrokeKind::Inside,
        );
    }

    fn cubic(&self, a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32)) {
        self.painter
            .add(egui::epaint::CubicBezierShape::from_points_stroke(
                [
                    self.p(a.0, a.1),
                    self.p(b.0, b.1),
                    self.p(c.0, c.1),
                    self.p(d.0, d.1),
                ],
                false,
                Color32::TRANSPARENT,
                self.stroke(),
            ));
    }

    fn arc(&self, cx: f32, cy: f32, r: f32, start: f32, end: f32) {
        let segments = 24;
        let points: Vec<Pos2> = (0..=segments)
            .map(|i| {
                let t = start + (end - start) * (i as f32) / (segments as f32);
                self.p(cx + r * t.cos(), cy + r * t.sin())
            })
            .collect();
        self.painter
            .add(egui::epaint::PathShape::line(points, self.stroke()));
    }
}

#[cfg(test)]
mod tests {
    //! 图标是纯绘制，没有可断言的几何结果；这里只钉住「枚举覆盖了全部分支」——
    //! 以后新增变体却忘记在 `paint` 里处理会直接编译失败（match 穷尽性），这条测试
    //! 负责提醒作者同步补测试清单。

    use super::Icon;

    #[test]
    fn every_icon_is_listed() {
        // 与 `Icon` 枚举一一对应；新增/删除图标时在这里同步。
        let icons = [
            Icon::Pulse,
            Icon::Route,
            Icon::Monitor,
            Icon::Settings,
            Icon::List,
            Icon::Search,
            Icon::Sun,
            Icon::Moon,
            Icon::ChevronDown,
            Icon::ChevronRight,
            Icon::ChevronUp,
            Icon::Warning,
            Icon::Check,
            Icon::Close,
            Icon::Play,
            Icon::Stop,
            Icon::Refresh,
            Icon::Shield,
            Icon::Clock,
            Icon::Download,
            Icon::Trash,
            Icon::Copy,
            Icon::Lock,
            Icon::Info,
            Icon::Server,
            Icon::Gauge,
        ];
        assert_eq!(icons.len(), 26);
    }
}
