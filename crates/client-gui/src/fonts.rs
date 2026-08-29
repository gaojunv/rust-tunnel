use std::sync::Arc;

use egui::{Context, FontData, FontDefinitions, FontFamily};

pub fn setup_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    if let Some((name, data)) = load_system_cjk_font() {
        tracing::info!("加载中文字体: {name}");
        fonts.font_data.insert(name.clone(), Arc::new(data));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, name.clone());
            } else {
                fonts.families.insert(family, vec![name.clone()]);
            }
        }
        ctx.set_fonts(fonts);
    } else {
        tracing::warn!("未找到系统中文字体，中文可能显示为方框");
    }
}

fn load_system_cjk_font() -> Option<(String, FontData)> {
    for (path, index) in cjk_candidates() {
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() < 1024 {
                continue;
            }
            let mut data = FontData::from_owned(bytes);
            data.index = index;
            let safe = path.replace(['/', '\\', ':', '.'], "_");
            let name = format!("cjk_{index}_{safe}");
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
