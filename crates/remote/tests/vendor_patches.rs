//! `vendor/shiguredo_libvpx` 补丁的机械锁测试。
//!
//! 远程控制数据面从 JPEG 瓦片改到 VP9 profile 1（8-bit 4:4:4），前提是上游 crate
//! 做不到、必须在 vendor 副本上打 5 处补丁（清单与动机见
//! `vendor/shiguredo_libvpx/src/lib.rs` 里的 `PATCH:` 注释）。这组测试断言
//! **补丁没有丢失**：上游升级 / rebase 时若把补丁冲掉，这里立刻红，而不是等到
//! 线上画质变糊、或跨平台解码器解不出来才发现。
//!
//! 覆盖的补丁：
//! - 补丁 1（`Vp9Profile::Profile1` + `g_profile = 1`）：`profile1_i444_encodes` 断言
//!   profile 1 + I444 能编码成功，且码流头里 `profile == 1`。
//! - 补丁 2（`Decoder::next_frame()` 接受 I444）：`profile1_i444_decodes_full_chroma`
//!   断言解码器不再对 I444 返回 `VPX_CODEC_UNSUP_FEATURE`。
//! - 补丁 3（`u_plane()`/`v_plane()` 按真实色度尺寸切片）：断言色度平面是全高
//!   （按 stride 算），而不是 4:2:0 的一半。
//! - 补丁 4（`rc_buf_sz`）：编码配置里设置 `rc_buffer_size_ms` 后 `Encoder::new`
//!   仍成功（control 路径不被上游结构体变更破坏）。
//! - 补丁 5（`VP9E_SET_COLOR_SPACE` / `VP9E_SET_COLOR_RANGE`）：断言码流头里的
//!   `color_space` / `color_range` 与配置一致，而不是恒为 `CS_UNKNOWN` + limited。

// 测试代码豁免 panic 风险 lint（与仓库既有集成测试一致）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::pedantic)]
#![allow(missing_docs)]

use std::num::NonZeroUsize;

use shiguredo_libvpx::{
    CodecConfig, ColorRange, ColorSpace, Decoder, DecoderCodec, DecoderConfig, EncodeOptions,
    Encoder, EncoderConfig, EncodingDeadline, ImageData, ImageFormat, Vp9Config, Vp9Profile,
};

/// 帧尺寸取小值，保证测试秒级完成（瓦片/差分逻辑不在此测试范围）。
const WIDTH: usize = 64;
const HEIGHT: usize = 64;

// ===========================================================================
// VP9 未压缩帧头解析（MSB-first 位流）
// ===========================================================================

/// 关心的字段。布局来源：VP9 Bitstream & Decoding Process Specification v0.6
/// 的 `uncompressed_header()` / `color_config()`。
#[derive(Debug, Clone, Copy)]
struct Vp9Header {
    /// `(profile_high_bit << 1) | profile_low_bit`
    profile: u8,
    /// `color_space` f(3)：2 = CS_BT_709，7 = CS_RGB
    color_space: u32,
    /// `color_range` f(1)：0 = studio/limited，1 = full
    color_range: u32,
    /// 仅 profile 1/3 显式信令；profile 0 恒为 1（4:2:0）
    subsampling_x: u32,
    subsampling_y: u32,
}

/// MSB-first 位读取器。
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    /// 读 `n` 位无符号整数（f(n)，MSB first）。
    fn f(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.data[self.bit / 8];
            let shift = 7 - (self.bit % 8);
            let b = (byte >> shift) & 1;
            v = (v << 1) | u32::from(b);
            self.bit += 1;
        }
        v
    }
}

/// 解析 VP9 未压缩帧头（要求输入是**关键帧**，否则返回 `None`）。
///
/// 字节 0：`frame_marker f(2) == 0b10` → `profile_low_bit f(1)` →
/// `profile_high_bit f(1)` → `Profile = (high << 1) | low`。
fn parse_vp9_header(data: &[u8]) -> Option<Vp9Header> {
    assert!(data.len() >= 8, "bitstream too short: {} bytes", data.len());
    let mut br = BitReader::new(data);

    assert_eq!(br.f(2), 0b10, "bad frame_marker");
    let profile_low_bit = br.f(1);
    let profile_high_bit = br.f(1);
    let profile = ((profile_high_bit << 1) | profile_low_bit) as u8;
    if profile == 3 {
        let _reserved_zero = br.f(1);
    }

    let show_existing_frame = br.f(1);
    if show_existing_frame == 1 {
        return None;
    }

    let frame_type = br.f(1); // 0 = KEY_FRAME
    let _show_frame = br.f(1);
    let _error_resilient_mode = br.f(1);
    if frame_type != 0 {
        return None;
    }

    let sync = br.f(24);
    assert_eq!(sync, 0x498342, "frame_sync_code mismatch: {sync:#x}");

    // color_config()
    if profile >= 2 {
        let _ten_or_twelve_bit = br.f(1);
    }
    let color_space = br.f(3);
    let (color_range, subsampling_x, subsampling_y) = if color_space != 7 {
        // != CS_RGB
        let color_range = br.f(1);
        if profile == 1 || profile == 3 {
            let ss_x = br.f(1);
            let ss_y = br.f(1);
            let _reserved_zero = br.f(1);
            (color_range, ss_x, ss_y)
        } else {
            (color_range, 1, 1)
        }
    } else {
        let _reserved_zero = br.f(1);
        (1, 0, 0)
    };

    Some(Vp9Header {
        profile,
        color_space,
        color_range,
        subsampling_x,
        subsampling_y,
    })
}

// ===========================================================================
// 编码辅助
// ===========================================================================

/// 用给定的 VP9 配置编一个 64×64 的关键帧，返回码流字节。
///
/// 输入是确定性渐变图案（非纯色），避免编码器把它当成退化输入。
fn encode_keyframe(profile: Vp9Profile, color_space: Option<ColorSpace>) -> Vec<u8> {
    let vp9 = Vp9Config {
        profile,
        color_space,
        // 补丁 5：同时验证 color_range 的口子能通到 libvpx。
        color_range: Some(ColorRange::Full),
        ..Default::default()
    };
    let mut config = EncoderConfig::new(WIDTH, HEIGHT, ImageFormat::I444, CodecConfig::Vp9(vp9));
    config.target_bitrate = 2_000_000;
    config.deadline = EncodingDeadline::Realtime;
    // `lag_in_frames = None` 只是「不覆盖」，libvpx 的默认 `g_lag_in_frames = 25`
    // 会把前 25 帧全缓冲住、一帧都不吐；实时路径必须显式写 1（零 lookahead）。
    config.lag_in_frames = Some(NonZeroUsize::new(1).unwrap());
    // 补丁 4：`rc_buffer_size_ms` 必须能在 Encoder::new 前设置且被 libvpx 接受。
    config.rc_buffer_size_ms = Some(NonZeroUsize::new(200).unwrap());

    let mut encoder = Encoder::new(config).expect("failed to create VP9 encoder");

    // I444：三个平面都是全尺寸；align=1 保证 stride == 宽。
    let mut y = vec![0u8; WIDTH * HEIGHT];
    for (i, px) in y.iter_mut().enumerate() {
        *px = ((i * 7) % 256) as u8;
    }
    let u = vec![128u8; WIDTH * HEIGHT];
    let v = vec![128u8; WIDTH * HEIGHT];

    encoder
        .encode(
            &ImageData::I444 { y: &y, u: &u, v: &v },
            &EncodeOptions {
                force_keyframe: true,
            },
        )
        .expect("encode() must succeed for profile 1 + I444 (补丁 1)");

    let packet = encoder
        .next_frame()
        .expect("encoder produced no packet")
        .data()
        .to_vec();
    assert!(!packet.is_empty(), "encoded packet is empty");
    packet
}

// ===========================================================================
// 测试
// ===========================================================================

/// 补丁 1 + 5：profile 1 + I444 能编码，且码流头里的 profile / 色度采样 /
/// 色彩元数据与配置一致。
#[test]
fn profile1_i444_encodes_with_expected_header() {
    let packet = encode_keyframe(Vp9Profile::Profile1, Some(ColorSpace::Bt709));
    let header = parse_vp9_header(&packet).expect("first packet must be a keyframe");

    // 补丁 1：g_profile = 1（不是 0/2）。
    assert_eq!(header.profile, 1, "bitstream profile must be 1");
    // 4:4:4：无色度子采样。
    assert_eq!(header.subsampling_x, 0, "ss_x must be 0 for 4:4:4");
    assert_eq!(header.subsampling_y, 0, "ss_y must be 0 for 4:4:4");
    // 补丁 5：BT.709 + full range 被写进码流，而不是 CS_UNKNOWN + limited。
    assert_eq!(header.color_space, 2, "color_space must be CS_BT_709");
    assert_eq!(header.color_range, 1, "color_range must be full range");
}

/// 补丁 1 的必要性：同样的 I444 输入换成 profile 0 必须失败。
///
/// libvpx 不会从 img fmt 反推 profile（`vp9/encoder/vp9_encoder.c` 对 profile 0/2
/// 拒绝非 4:2:0），所以「profile 1 能编」这件事本身就是补丁生效的证据。
#[test]
fn profile0_i444_encode_fails() {
    let config = EncoderConfig::new(
        WIDTH,
        HEIGHT,
        ImageFormat::I444,
        CodecConfig::Vp9(Vp9Config {
            profile: Vp9Profile::Profile0,
            ..Default::default()
        }),
    );
    let mut encoder = Encoder::new(config).expect("failed to create VP9 encoder");

    let y = vec![0u8; WIDTH * HEIGHT];
    let u = vec![128u8; WIDTH * HEIGHT];
    let v = vec![128u8; WIDTH * HEIGHT];

    let result = encoder.encode(
        &ImageData::I444 { y: &y, u: &u, v: &v },
        &EncodeOptions {
            force_keyframe: true,
        },
    );
    assert!(
        result.is_err(),
        "profile 0 + I444 must fail; if it succeeds, libvpx changed and 补丁 1 可重新评估"
    );
}

/// 补丁 2 + 3：解码器接受 I444，且色度平面是全尺寸（不是 4:2:0 的一半）。
#[test]
fn profile1_i444_decodes_full_chroma() {
    let packet = encode_keyframe(Vp9Profile::Profile1, None);

    let mut decoder = Decoder::new(DecoderConfig {
        codec: DecoderCodec::Vp9,
    })
    .expect("failed to create decoder");
    decoder
        .decode(&packet)
        .expect("decode() must accept a profile 1 stream");

    let frame = decoder
        .next_frame()
        .expect("next_frame() must not error for I444 (补丁 2)")
        .expect("decoder produced no frame");

    assert_eq!(frame.width(), WIDTH);
    assert_eq!(frame.height(), HEIGHT);

    // 补丁 3：色度切片长度按 stride 算，且是全高。
    let u_stride = frame.u_stride();
    let v_stride = frame.v_stride();
    assert!(
        u_stride >= WIDTH && v_stride >= WIDTH,
        "libvpx strides are expected to be at least the width (u={u_stride}, v={v_stride})"
    );
    assert_eq!(
        frame.u_plane().len(),
        HEIGHT * u_stride,
        "U plane must be full height (补丁 3)"
    );
    assert_eq!(
        frame.v_plane().len(),
        HEIGHT * v_stride,
        "V plane must be full height (补丁 3)"
    );
    // 与「写死 4:2:0」的旧行为明确区分：旧代码只切一半。
    assert!(
        frame.u_plane().len() > (HEIGHT / 2) * u_stride,
        "U plane must be larger than the 4:2:0 half-height slice"
    );
}
