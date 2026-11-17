//! 字体装配：系统 UI 字体优先（Windows 用 Segoe UI），再补中文字体与等宽字体。
//!
//! egui 的默认正文字体是 Ubuntu Light，放在 Win11 上明显不像系统应用。这里按平台加载
//! **系统自带的 UI 字体**：
//! - Windows：`segoeui.ttf`（正文）、`seguisb.ttf`（半粗，标题）、`consola.ttf`（等宽）、
//!   `msyh.ttc` / `msyhbd.ttc`（中文正文/粗体）；
//! - macOS：SF / Helvetica / PingFang；
//! - Linux：Noto Sans / DejaVu Sans Mono 等。
//!
//! `egui` 没有字重概念，`RichText::strong()` 只改颜色。为了真正做出 Fluent 的标题层次，
//! 这里额外注册一个 `heading` 字体家族（半粗拉丁 + 粗体中文 + 正文兜底），由
//! [`heading_family`] 返回；找不到半粗字体时回退正文家族。

use std::sync::{Arc, OnceLock};

use egui::{Context, FontData, FontDefinitions, FontFamily};

/// 半粗标题家族的内部名字。
const HEADING_FAMILY: &str = "heading";

/// `setup_fonts` 是否成功注册了半粗家族（`heading_family` 据此回退）。
static HAS_HEADING: OnceLock<bool> = OnceLock::new();

/// 标题字体家族：有半粗字体就用它，否则回退正文字体。
#[must_use]
pub fn heading_family() -> FontFamily {
    if *HAS_HEADING.get_or_init(|| false) {
        FontFamily::Name(HEADING_FAMILY.into())
    } else {
        FontFamily::Proportional
    }
}

/// 装配字体并写入 `ctx`。
pub fn setup_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();

    let latin = load_first(latin_candidates());
    let latin_semibold = load_first(latin_semibold_candidates());
    let mono = load_first(mono_candidates());
    let cjk = load_first(cjk_candidates());
    let cjk_bold = load_first(cjk_bold_candidates());

    for font in [
        latin.as_ref(),
        latin_semibold.as_ref(),
        mono.as_ref(),
        cjk.as_ref(),
        cjk_bold.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        fonts
            .font_data
            .insert(font.0.clone(), Arc::new(font.1.clone()));
    }

    // 正文：系统拉丁 → 中文 → egui 自带的 emoji / 兜底。
    let mut proportional = Vec::new();
    push_name(&mut proportional, latin.as_ref());
    push_name(&mut proportional, cjk.as_ref());
    append_default(&mut fonts, FontFamily::Proportional, proportional);

    // 等宽：系统等宽 → 中文（日志里的中文不走豆腐块）→ 兜底。
    let mut monospace = Vec::new();
    push_name(&mut monospace, mono.as_ref());
    push_name(&mut monospace, cjk.as_ref());
    append_default(&mut fonts, FontFamily::Monospace, monospace);

    // 标题：半粗拉丁 → 粗体中文 → 正文拉丁 → 中文 → emoji 兜底。
    let mut heading = Vec::new();
    push_name(&mut heading, latin_semibold.as_ref());
    push_name(&mut heading, cjk_bold.as_ref());
    push_name(&mut heading, latin.as_ref());
    push_name(&mut heading, cjk.as_ref());
    let has_heading = !heading.is_empty();
    if has_heading {
        let fallback = fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        heading.extend(fallback);
        fonts
            .families
            .insert(FontFamily::Name(HEADING_FAMILY.into()), heading);
    }
    let _ = HAS_HEADING.set(has_heading);

    if let Some((name, _)) = latin.as_ref() {
        tracing::info!("加载系统 UI 字体: {name}");
    }
    if let Some((name, _)) = cjk.as_ref() {
        tracing::info!("加载中文字体: {name}");
    } else {
        tracing::warn!("未找到系统中文字体，中文可能显示为方框");
    }
    ctx.set_fonts(fonts);
}

fn push_name(list: &mut Vec<String>, font: Option<&(String, FontData)>) {
    if let Some((name, _)) = font {
        if !list.contains(name) {
            list.push(name.clone());
        }
    }
}

fn append_default(fonts: &mut FontDefinitions, family: FontFamily, front: Vec<String>) {
    let fallback = fonts.families.get(&family).cloned().unwrap_or_default();
    let mut list = front;
    for name in fallback {
        if !list.contains(&name) {
            list.push(name);
        }
    }
    fonts.families.insert(family, list);
}

/// 按候选顺序读取第一个能解析的字体。
fn load_first(candidates: Vec<(String, u32)>) -> Option<(String, FontData)> {
    for (path, index) in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() < 1024 {
                continue;
            }
            let mut data = FontData::from_owned(bytes);
            data.index = index;
            let safe = path.replace(['/', '\\', ':', '.'], "_");
            let name = format!("sys_{index}_{safe}");
            if validate_font_bytes(&data) {
                return Some((name, data));
            }
        }
    }
    None
}

fn validate_font_bytes(data: &FontData) -> bool {
    let bytes: &[u8] = match &data.font {
        std::borrow::Cow::Borrowed(b) => b,
        std::borrow::Cow::Owned(b) => b,
    };
    if bytes.len() < 4 {
        return false;
    }
    matches!(
        &bytes[0..4],
        b"ttcf" | b"OTTO" | b"true" | b"\x00\x01\x00\x00"
    ) || (bytes[0] == 0 && bytes[1] == 1)
}

fn latin_candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        v.push(("C:\\Windows\\Fonts\\segoeui.ttf".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\SegUIVar.ttf".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\tahoma.ttf".to_string(), 0));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(("/System/Library/Fonts/SFNS.ttf".to_string(), 0));
        v.push(("/System/Library/Fonts/Helvetica.ttc".to_string(), 0));
        v.push(("/System/Library/Fonts/Geneva.ttf".to_string(), 0));
    }
    #[cfg(target_os = "linux")]
    {
        for p in [
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ] {
            v.push((p.to_string(), 0));
        }
    }
    v
}

fn latin_semibold_candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        v.push(("C:\\Windows\\Fonts\\seguisb.ttf".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\segoeuib.ttf".to_string(), 0));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(("/System/Library/Fonts/SFNS.ttf".to_string(), 0));
    }
    #[cfg(target_os = "linux")]
    {
        for p in [
            "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ] {
            v.push((p.to_string(), 0));
        }
    }
    v
}

fn mono_candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        v.push(("C:\\Windows\\Fonts\\consola.ttf".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\cour.ttf".to_string(), 0));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(("/System/Library/Fonts/Menlo.ttc".to_string(), 0));
        v.push(("/System/Library/Fonts/Monaco.ttf".to_string(), 0));
    }
    #[cfg(target_os = "linux")]
    {
        for p in [
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        ] {
            v.push((p.to_string(), 0));
        }
    }
    v
}

fn cjk_candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Some(p) = find_pingfang() {
            v.push((p, 3));
        }
        v.push(("/System/Library/Fonts/Hiragino Sans GB.ttc".to_string(), 0));
        v.push(("/System/Library/Fonts/STHeiti Light.ttc".to_string(), 1));
        v.push((
            "/System/Library/Fonts/Supplemental/Songti.ttc".to_string(),
            6,
        ));
    }
    #[cfg(target_os = "windows")]
    {
        v.push(("C:\\Windows\\Fonts\\msyh.ttc".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\msyahei.ttc".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\simhei.ttf".to_string(), 0));
        v.push(("C:\\Windows\\Fonts\\simsun.ttc".to_string(), 0));
    }
    #[cfg(target_os = "linux")]
    {
        for p in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansSC-Regular.otf",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansSC-Regular.ttf",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ] {
            v.push((p.to_string(), 0));
        }
    }
    if !cfg!(target_os = "macos") {
        v.push(("/System/Library/Fonts/Hiragino Sans GB.ttc".to_string(), 0));
        v.push(("/Library/Fonts/Arial Unicode.ttf".to_string(), 0));
    }
    v
}

fn cjk_bold_candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        v.push(("C:\\Windows\\Fonts\\msyhbd.ttc".to_string(), 0));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(("/System/Library/Fonts/STHeiti Medium.ttc".to_string(), 1));
    }
    #[cfg(target_os = "linux")]
    {
        for p in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansSC-Bold.otf",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ] {
            v.push((p.to_string(), 0));
        }
    }
    v
}

#[cfg(target_os = "macos")]
fn find_pingfang() -> Option<String> {
    let base = "/System/Library/AssetsV2/com_apple_MobileAsset_Font8";
    let entries = std::fs::read_dir(base).ok()?;
    for e in entries.flatten() {
        let p = e.path().join("AssetData/PingFang.ttc");
        if p.is_file() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{heading_family, FontData};

    #[test]
    fn heading_family_is_named_or_proportional() {
        // 单测不装配字体：`heading_family` 必须安全回退，不能 panic。
        let family = heading_family();
        assert!(
            family == egui::FontFamily::Proportional || matches!(family, egui::FontFamily::Name(_))
        );
    }

    #[test]
    fn accepts_common_font_magics() {
        for magic in [*b"ttcf", *b"OTTO", *b"true", [0, 1, 0, 0]] {
            let data = FontData::from_owned(magic.to_vec());
            assert!(super::validate_font_bytes(&data));
        }
    }
}
