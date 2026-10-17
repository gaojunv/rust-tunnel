//! 像素格式转换，以及「用纯文本就能验画面」的三件工具（可移植）。
//!
//! # 为什么需要这一层
//!
//! GDI 给的 32 位 DIB（`BI_RGB`）在内存里是 **BGRA**，而远程控制引擎要的是 **RGBA**。
//! 搞错分量顺序的后果是红蓝互换——画面「看着像那么回事」，很容易被误判成 VP9 编解码
//! 的 bug，所以转换在这里显式做、并且有单测钉着。
//!
//! 行序则**不在这里翻**：采集侧直接把 `BITMAPINFOHEADER.biHeight` 设成负数，
//! GDI 就会交出**自顶向下**的缓冲（见 [`crate::capture`]）。
//!
//! # 另外两件工具为什么存在
//!
//! 阶段 0 的探针只能靠「人把日志贴回来」验收，而日志是文本。于是需要
//! [`ascii_thumbnail`]（把画面降采样成字符画，肉眼判断「是不是拍到了东西、方向对不对」）
//! 与 [`encode_bmp`]（落一个真图，用 Windows 上的看图工具确认色彩通道）。
//! 注意两者的**来源不同**：BMP 直接写**采集到的原始 BGRA 字节**，ASCII 缩略图来自
//! **转换后的 RGBA**。这是刻意的——如果 BMP 也从转换后的缓冲生成，那么转换里的分量
//! 顺序错误会在「转过去再转回来」中相互抵消、什么也验不出来。

/// 每像素字节数（RGBA 与 BGRA 都是 4）。
pub const BYTES_PER_PIXEL: usize = 4;

/// 不透明像素的 alpha 值。
///
/// GDI 的高字节是**未定义**的（实测常常是 0），照抄进 RGBA 会让纹理全透明，
/// 故一律填满。
pub const OPAQUE_ALPHA: u8 = u8::MAX;

/// BGRA → RGBA，**就地追加**到 `dst`（先清空）。
///
/// `src` 末尾不足 4 字节的残渣会被丢弃（正常不会出现：行宽由 GDI 按 4 字节对齐给出）。
pub fn bgra_to_rgba_into(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len());
    // `as_chunks` 已经在编译期保证每块正好 4 字节（残渣单独返回）：
    // 比 `chunks_exact` 少一次长度判断。
    let (pixels, _trailing) = src.as_chunks::<BYTES_PER_PIXEL>();
    for px in pixels {
        dst.push(px[2]);
        dst.push(px[1]);
        dst.push(px[0]);
        dst.push(OPAQUE_ALPHA);
    }
}

/// BGRA → RGBA，返回新缓冲。逐帧路径请用 [`bgra_to_rgba_into`] 复用缓冲。
#[must_use]
pub fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    bgra_to_rgba_into(src, &mut out);
    out
}

/// 把 RGBA 画面降采样成字符画：**每行以 `\n` 结尾**，最后一行也有。
///
/// `cols`/`rows` 是字符画尺寸（会被夹到不超过画面本身的宽高，不做放大）；
/// 任一参数为 0 时返回空字符串。每个字符取对应小块的**平均亮度**，亮度用
/// BT.601 系数（`0.299R + 0.587G + 0.114B`，整数运算）。
///
/// 用途是「把画面贴进日志」——能看出是否全黑、是否有结构、上下有没有翻。
#[must_use]
pub fn ascii_thumbnail(rgba: &[u8], width: u32, height: u32, cols: u32, rows: u32) -> String {
    const RAMP: &[u8] = b" .:-=+*#%@";

    if width == 0 || height == 0 || cols == 0 || rows == 0 {
        return String::new();
    }
    let cols = cols.min(width);
    let rows = rows.min(height);
    let row_bytes = usize::try_from(width).unwrap_or(0) * BYTES_PER_PIXEL;
    if rgba.len() < row_bytes * usize::try_from(height).unwrap_or(0) {
        return String::new();
    }

    let (cw, ch) = (u64::from(width), u64::from(height));
    let mut out = String::with_capacity(
        usize::try_from(rows).unwrap_or(0) * (usize::try_from(cols).unwrap_or(0) + 1),
    );

    for row in 0..u64::from(rows) {
        let y0 = row * ch / u64::from(rows);
        let y1 = ((row + 1) * ch / u64::from(rows)).max(y0 + 1);
        for col in 0..u64::from(cols) {
            let x0 = col * cw / u64::from(cols);
            let x1 = ((col + 1) * cw / u64::from(cols)).max(x0 + 1);

            let (mut sum_r, mut sum_g, mut sum_b, mut count) = (0_u64, 0_u64, 0_u64, 0_u64);
            for y in y0..y1 {
                let base = usize::try_from(y).unwrap_or(0) * row_bytes;
                for x in x0..x1 {
                    let off = base + usize::try_from(x).unwrap_or(0) * BYTES_PER_PIXEL;
                    sum_r += u64::from(rgba[off]);
                    sum_g += u64::from(rgba[off + 1]);
                    sum_b += u64::from(rgba[off + 2]);
                    count += 1;
                }
            }
            let luma = if count == 0 {
                0
            } else {
                (299 * sum_r + 587 * sum_g + 114 * sum_b) / (1000 * count)
            };
            let idx = usize::try_from(luma * (RAMP.len() as u64 - 1) / 255).unwrap_or(0);
            out.push(char::from(RAMP[idx.min(RAMP.len() - 1)]));
        }
        out.push('\n');
    }
    out
}

/// 把**自顶向下**的 32 位 BGRA 画面编码成一张合法的 32 位 BMP（自底向上）。
///
/// `bgra` 的长度不足 `width * height * 4` 时返回空 `Vec`（宁可产出空文件，
/// 也不要产出一张尺寸对不上的图）。
///
/// 32 位 BMP 的行本就 4 字节对齐，故没有行补齐问题。
///
/// # 唯一一处改动：alpha 字节统一写 255
///
/// GDI 采出来的 DIB 里那个字节一直是 0，于是这张 BMP 整张都是「全透明」。多数
/// 查看器不看它，但**honor 它的那些会把图显示成全黑或全透明**——而这张图正是
/// 用来证明「通道顺序与上下方向没搞反」的凭据，被误读成「采集失败」就白跑了。
/// **只动第 4 个字节**，B/G/R 三个字节逐字节原样，通道顺序的验证价值一点没少。
#[must_use]
pub fn encode_bmp(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    const FILE_HEADER_LEN: u32 = 14;
    const INFO_HEADER_LEN: u32 = 40;
    const HEADER_LEN: u32 = FILE_HEADER_LEN + INFO_HEADER_LEN;

    if width == 0 || height == 0 {
        return Vec::new();
    }
    let row_bytes = usize::try_from(width).unwrap_or(0) * BYTES_PER_PIXEL;
    let expected = row_bytes * usize::try_from(height).unwrap_or(0);
    if bgra.len() < expected {
        return Vec::new();
    }
    let image_bytes = u32::try_from(expected).unwrap_or(0);
    if image_bytes == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(usize::try_from(HEADER_LEN).unwrap_or(0) + expected);

    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&HEADER_LEN.saturating_add(image_bytes).to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // bfReserved1
    out.extend_from_slice(&0_u16.to_le_bytes()); // bfReserved2
    out.extend_from_slice(&HEADER_LEN.to_le_bytes()); // bfOffBits

    // BITMAPINFOHEADER
    out.extend_from_slice(&INFO_HEADER_LEN.to_le_bytes()); // biSize
    out.extend_from_slice(&i32::try_from(width).unwrap_or(0).to_le_bytes()); // biWidth
                                                                             // 正数 = 自底向上，BMP 的常规布局
    out.extend_from_slice(&i32::try_from(height).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32_u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0_u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&image_bytes.to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0_i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0_i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrImportant

    // 像素：输入自顶向下，BMP 要自底向上，故按行倒序写出；顺带把 alpha 字节
    // 统一成 255（理由见函数文档，只动第 4 个字节）。
    for row in (0..usize::try_from(height).unwrap_or(0)).rev() {
        let start = row * row_bytes;
        for i in (start..start + row_bytes).step_by(BYTES_PER_PIXEL) {
            out.extend_from_slice(&bgra[i..i + 3]);
            out.push(0xFF);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_is_reordered_to_rgba_and_alpha_filled() {
        // 两个像素：BGRA(1,2,3,0) BGRA(4,5,6,255)
        let src = [1_u8, 2, 3, 0, 4, 5, 6, 255];
        let got = bgra_to_rgba(&src);
        assert_eq!(got, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn bgra_conversion_reuses_the_target_buffer() {
        let mut buf = vec![9_u8; 64];
        bgra_to_rgba_into(&[1, 2, 3, 4], &mut buf);
        assert_eq!(buf, vec![3, 2, 1, 255]);
    }

    #[test]
    fn bgra_conversion_drops_trailing_partial_pixel() {
        let got = bgra_to_rgba(&[1, 2, 3, 0, 7, 7]);
        assert_eq!(got.len(), BYTES_PER_PIXEL);
    }

    /// 一张 4x4 的图，左半黑右半白 → 字符画应当是「左半空格、右半 @」。
    #[test]
    fn ascii_thumbnail_maps_black_and_white_to_ramp_ends() {
        let (w, h) = (4_u32, 4_u32);
        let mut rgba = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                let v = if x < 2 { 0 } else { 255 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let art = ascii_thumbnail(&rgba, w, h, 4, 2);
        assert_eq!(art, "  @@\n  @@\n");
    }

    #[test]
    fn ascii_thumbnail_is_luma_weighted_not_channel_average() {
        // 纯绿在 BT.601 下亮度是 150（不是 255），落在 ramp 的中段而不是末尾。
        let rgba = [0_u8, 255, 0, 255];
        let art = ascii_thumbnail(&rgba, 1, 1, 1, 1);
        assert_ne!(art.trim(), "@");
        assert_ne!(art.trim(), "");
    }

    #[test]
    fn ascii_thumbnail_degrades_gracefully() {
        assert_eq!(ascii_thumbnail(&[], 0, 0, 8, 8), "");
        assert_eq!(ascii_thumbnail(&[0; 16], 2, 2, 0, 8), "");
        // 缓冲比声明的尺寸小：不 panic，也不产出半张图。
        assert_eq!(ascii_thumbnail(&[0; 16], 4, 4, 4, 4), "");
    }

    #[test]
    fn ascii_thumbnail_does_not_upsample() {
        // 要 8 列但只有 2 列像素 → 只出 2 列。
        let rgba = vec![255_u8; 2 * 2 * BYTES_PER_PIXEL];
        let art = ascii_thumbnail(&rgba, 2, 2, 8, 8);
        assert_eq!(art, "@@\n@@\n");
    }

    #[test]
    fn bmp_header_is_well_formed() {
        let (w, h) = (2_u32, 3_u32);
        let bgra: Vec<u8> = (0..u8::try_from(w * h * 4).unwrap()).collect();
        let bmp = encode_bmp(&bgra, w, h);

        assert_eq!(&bmp[0..2], b"BM");
        let file_size = u32::from_le_bytes([bmp[2], bmp[3], bmp[4], bmp[5]]);
        assert_eq!(file_size as usize, bmp.len());
        let off_bits = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]);
        assert_eq!(off_bits, 54);
        let info_size = u32::from_le_bytes([bmp[14], bmp[15], bmp[16], bmp[17]]);
        assert_eq!(info_size, 40);
        let width = i32::from_le_bytes([bmp[18], bmp[19], bmp[20], bmp[21]]);
        let height = i32::from_le_bytes([bmp[22], bmp[23], bmp[24], bmp[25]]);
        assert_eq!((width, height), (2, 3), "高度必须为正（自底向上）");
        let bpp = u16::from_le_bytes([bmp[28], bmp[29]]);
        assert_eq!(bpp, 32);
        assert_eq!(bmp.len(), 54 + usize::try_from(w * h * 4).unwrap());
    }

    #[test]
    fn bmp_rows_are_written_bottom_up() {
        // 3 行各填一个特征字节，第 0 行=0xAA、第 2 行=0xCC。
        let (w, h) = (1_u32, 3_u32);
        let mut bgra = Vec::new();
        for row in [0xAA_u8, 0xBB, 0xCC] {
            bgra.extend_from_slice(&[row, row, row, 255]);
        }
        let bmp = encode_bmp(&bgra, w, h);
        let pixels = &bmp[54..];
        assert_eq!(pixels[0], 0xCC, "文件里第一个像素应当是输入的**最后**一行");
        assert_eq!(pixels[8], 0xAA);
    }

    #[test]
    fn bmp_normalizes_alpha_but_leaves_bgr_untouched() {
        // GDI 采出来的 DIB 里 alpha 恒为 0；写进 BMP 必须改成 255，否则会「honor
        // alpha」的看图工具会把这张**证据图**显示成全透明，被误读成「采集失败」。
        // B/G/R 三个字节必须逐字节原样——通道顺序就是靠它们验的。
        let bgra = [1_u8, 2, 3, 0, 4, 5, 6, 0];
        let bmp = encode_bmp(&bgra, 2, 1);
        assert_eq!(&bmp[54..], &[1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn bmp_rejects_short_input_instead_of_writing_a_broken_file() {
        assert!(encode_bmp(&[0; 8], 4, 4).is_empty());
        assert!(encode_bmp(&[], 0, 10).is_empty());
    }
}
