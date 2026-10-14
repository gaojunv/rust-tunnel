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
//! - 补丁 6（`Encoder::set_bitrate` / `set_fps`，即 `vpx_codec_enc_config_set`）：
//!   `runtime_bitrate_change_does_not_force_keyframe` 断言在线改码率后不产生 IDR、
//!   码流仍可解；`runtime_fps_change_is_accepted` 断言在线改帧率被 libvpx 接受。
//! - 补丁 7（`DecoderConfig::threads` / `row_mt`，即 `vpx_codec_dec_cfg_t` +
//!   `VP9D_SET_ROW_MT`）：`multithreaded_row_mt_decode_is_bit_exact` 断言同一段码流用
//!   「单线程 + 关 row-mt」与「多线程 + 开 row-mt」解出的平面**逐字节相同**（并行不改变
//!   解码结果）；`encoder_accepts_tile_columns_and_row_mt` 断言编码器接受
//!   `tile_columns` + `row_mt` 并产出可解码流。

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

    let mut decoder =
        Decoder::new(DecoderConfig::new(DecoderCodec::Vp9)).expect("failed to create decoder");
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

/// 造一个 64×64 的 profile 1 / I444 编码器（供补丁 6 的用例复用）。
///
/// 与 [`encode_keyframe`] 的区别：这里返回编码器本身，供用例在两次 `encode` 之间调用
/// 运行期重配置。
fn runtime_encoder() -> Encoder {
    let vp9 = Vp9Config {
        profile: Vp9Profile::Profile1,
        color_space: Some(ColorSpace::Bt709),
        color_range: Some(ColorRange::Full),
        ..Default::default()
    };
    let mut config = EncoderConfig::new(WIDTH, HEIGHT, ImageFormat::I444, CodecConfig::Vp9(vp9));
    config.target_bitrate = 2_000_000;
    config.deadline = EncodingDeadline::Realtime;
    config.lag_in_frames = Some(NonZeroUsize::new(1).unwrap());
    config.rc_buffer_size_ms = Some(NonZeroUsize::new(300).unwrap());
    Encoder::new(config).expect("failed to create VP9 encoder")
}

/// 造一对 I444 平面（内容随 `shade` 变化，避免第二帧退化成「无变化」）。
fn i444_planes(shade: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y = vec![0u8; WIDTH * HEIGHT];
    for (i, px) in y.iter_mut().enumerate() {
        *px = (((i * 7) % 256) as u8).wrapping_add(shade);
    }
    (y, vec![128u8; WIDTH * HEIGHT], vec![128u8; WIDTH * HEIGHT])
}

/// 编码一帧并取回码流字节；返回 `(字节, 是否关键帧)`。
///
/// **必须把 `next_frame()` 抽干**（抽到 `None` 才会重置内部迭代器），否则下一次 `encode()`
/// 会报 "still need to call next_frame()"。
fn encode_packet(encoder: &mut Encoder, shade: u8, force_keyframe: bool) -> (Vec<u8>, bool) {
    let (y, u, v) = i444_planes(shade);
    encoder
        .encode(
            &ImageData::I444 { y: &y, u: &u, v: &v },
            &EncodeOptions { force_keyframe },
        )
        .expect("encode() must succeed");
    let mut last = None;
    let mut keyframe = false;
    while let Some(packet) = encoder.next_frame() {
        keyframe |= packet.is_keyframe();
        last = Some(packet.data().to_vec());
    }
    (last.expect("encoder produced no packet"), keyframe)
}

/// 补丁 6：在线改码率**不**产生 IDR，且码流仍可解。
///
/// 这是 AIMD 闭环的前提：码率每秒动一次，若每次都重建编码器就会每秒一个关键帧，
/// 正好把阶段 2 要消掉的带宽尖峰引回来。断言 `set_bitrate` 走
/// `vpx_codec_enc_config_set` 的在线路径（而非重建）。
#[test]
fn runtime_bitrate_change_does_not_force_keyframe() {
    let mut encoder = runtime_encoder();
    assert_eq!(encoder.bitrate_kbps(), 2000, "初始码率应为 2 Mbps");

    // 首帧关键帧，建立参考链。
    let (keyframe, is_key) = encode_packet(&mut encoder, 0, true);
    assert!(is_key, "首帧应为关键帧");
    assert!(parse_vp9_header(&keyframe).is_some(), "首帧应为关键帧");

    // 在线改码率（补丁 6）。单位是 kbps。
    encoder.set_bitrate(700).expect("set_bitrate 必须成功（补丁 6）");
    assert_eq!(encoder.bitrate_kbps(), 700, "set_bitrate 后码率应更新");

    // 下一帧：不得是关键帧。
    let (pframe, is_key) = encode_packet(&mut encoder, 40, false);
    assert!(
        !is_key,
        "在线改码率不得插入 IDR（否则每秒一个关键帧，闭环收益被吃掉）"
    );

    // 码流仍可解（关键帧 → P 帧）。
    let mut decoder = Decoder::new(DecoderConfig::new(DecoderCodec::Vp9)).expect("decoder");
    decoder.decode(&keyframe).expect("decode keyframe");
    while decoder.next_frame().expect("frame").is_some() {}
    decoder.decode(&pframe).expect("decode P frame after set_bitrate");
    let frame = decoder.next_frame().expect("frame").expect("decoded frame");
    assert_eq!(frame.width(), WIDTH);
    assert_eq!(frame.height(), HEIGHT);
}

/// 补丁 6：在线改帧率被 libvpx 接受，且不产生 IDR。
#[test]
fn runtime_fps_change_is_accepted() {
    let mut encoder = runtime_encoder();
    let (keyframe, is_key) = encode_packet(&mut encoder, 0, true);
    assert!(is_key);
    assert!(parse_vp9_header(&keyframe).is_some());

    // 12 → 8 fps。
    encoder.set_fps(8).expect("set_fps 必须成功（补丁 6）");
    assert_eq!(encoder.fps(), 8, "set_fps 后帧率应更新");

    let (_pframe, is_key) = encode_packet(&mut encoder, 60, false);
    assert!(!is_key, "在线改帧率不得插入 IDR");
}

// ===========================================================================
// 补丁 7：解码侧并行（threads + row-mt）
// ===========================================================================

/// 补丁 7 的测试尺寸：小到秒级，又给瓦片留出余量（256 宽 → 2 列时每瓦片 128 像素）。
const PW: usize = 256;
/// 见 [`PW`]。
const PH: usize = 192;

/// 造一个开了 `tile_columns` + `row_mt` 的 I444 编码器（形状与 `video.rs` 的编码配置一致）。
///
/// `tile_columns = 1` 是 log2 的瓦片列数（即 2 列）——给解码器提供瓦片边界作为并行切分点。
fn tiled_encoder() -> Encoder {
    let vp9 = Vp9Config {
        profile: Vp9Profile::Profile1,
        color_space: Some(ColorSpace::Bt709),
        color_range: Some(ColorRange::Full),
        tile_columns: Some(1),
        row_mt: true,
        ..Default::default()
    };
    let mut config = EncoderConfig::new(PW, PH, ImageFormat::I444, CodecConfig::Vp9(vp9));
    config.target_bitrate = 2_000_000;
    config.deadline = EncodingDeadline::Realtime;
    config.lag_in_frames = Some(NonZeroUsize::new(1).unwrap());
    config.rc_buffer_size_ms = Some(NonZeroUsize::new(300).unwrap());
    Encoder::new(config).expect("failed to create VP9 encoder")
}

/// 造 PW×PH 的 I444 平面（内容随 `shade` 变化，避免第二帧退化成「无变化」）。
fn tiled_planes(shade: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y = vec![0u8; PW * PH];
    for (i, px) in y.iter_mut().enumerate() {
        *px = (((i * 7) % 256) as u8).wrapping_add(shade);
    }
    let mut u = vec![0u8; PW * PH];
    for (i, px) in u.iter_mut().enumerate() {
        *px = (((i * 3) % 200) as u8).wrapping_add(shade);
    }
    let mut v = vec![0u8; PW * PH];
    for (i, px) in v.iter_mut().enumerate() {
        *px = (((i * 5) % 200) as u8).wrapping_add(shade);
    }
    (y, u, v)
}

/// 编一段「1 个关键帧 + `frames - 1` 个 P 帧」的码流（编码器开了 tile_columns + row_mt），
/// 返回逐帧字节。
fn encode_tiled_stream(frames: usize) -> Vec<Vec<u8>> {
    let mut encoder = tiled_encoder();
    let mut packets = Vec::with_capacity(frames);
    for i in 0..frames {
        let (y, u, v) = tiled_planes(u8::try_from(i).unwrap_or(0).wrapping_mul(17));
        encoder
            .encode(
                &ImageData::I444 { y: &y, u: &u, v: &v },
                &EncodeOptions {
                    force_keyframe: i == 0,
                },
            )
            .expect("encode must succeed with tile_columns + row_mt");
        let mut last = None;
        while let Some(packet) = encoder.next_frame() {
            last = Some(packet.data().to_vec());
        }
        packets.push(last.expect("encoder produced no packet"));
    }
    packets
}

/// 用给定配置的解码器解完整段码流，返回每帧的「Y ‖ U ‖ V 平面字节」拼接。
///
/// 平面字节是解码输出里唯一会变的量；RGBA 是它的**确定性纯函数**（见
/// `video::i444_to_rgba`），故平面逐字节相同 ⟹ RGBA 逐字节相同。
fn decode_all(config: DecoderConfig, packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut decoder = Decoder::new(config).expect("failed to create decoder");
    let mut out = Vec::with_capacity(packets.len());
    for packet in packets {
        decoder.decode(packet).expect("decode must succeed");
        let mut frame_out = None;
        while let Some(frame) = decoder.next_frame().expect("next_frame") {
            let mut planes = Vec::new();
            planes.extend_from_slice(frame.y_plane());
            planes.extend_from_slice(frame.u_plane());
            planes.extend_from_slice(frame.v_plane());
            frame_out = Some(planes);
        }
        out.push(frame_out.expect("decoder produced no frame"));
    }
    out
}

/// 补丁 7 核心断言：**多线程 + row-mt 不改变解码结果**。
///
/// 同一段码流分别用「单线程 + 关 row-mt」（上游行为）与「多线程 + 开 row-mt」解，逐字节
/// 比对解码平面必须完全相同。耗时在 CI 上不可比，这条锁的是「并行没有改变解码结果」，
/// 比断言耗时可靠得多。
#[test]
fn multithreaded_row_mt_decode_is_bit_exact() {
    let packets = encode_tiled_stream(6);

    // 上游行为：threads = None（cfg = NULL）、row_mt = false。
    let single = DecoderConfig::new(DecoderCodec::Vp9);
    let single_out = decode_all(single, &packets);

    // 补丁 7：多线程 + 行级并行。
    let mut multi = DecoderConfig::new(DecoderCodec::Vp9);
    multi.threads = NonZeroUsize::new(4);
    multi.row_mt = true;
    let multi_out = decode_all(multi, &packets);

    assert_eq!(single_out.len(), packets.len(), "单线程侧应解出每一帧");
    assert_eq!(multi_out.len(), packets.len(), "多线程侧应解出每一帧");
    // 非退化：输出不应是空/全零（否则「相同」没有意义）。
    assert!(!multi_out[0].is_empty(), "解码输出不应为空");
    assert!(
        multi_out.iter().any(|p| p.iter().any(|&b| b != 0)),
        "解码输出不应全零"
    );
    assert_eq!(
        single_out, multi_out,
        "并行解码（threads + row-mt）必须与单线程逐字节一致"
    );
}

/// 补丁 7 smoke：编码器接受 `tile_columns` + `row_mt`，且产出的码流可被解码。
#[test]
fn encoder_accepts_tile_columns_and_row_mt() {
    let packets = encode_tiled_stream(2);
    assert_eq!(packets.len(), 2);
    assert!(!packets[0].is_empty(), "首帧（关键帧）不应为空");

    let mut decoder = Decoder::new(DecoderConfig::new(DecoderCodec::Vp9)).expect("decoder");
    decoder.decode(&packets[0]).expect("decode keyframe");
    let frame = decoder
        .next_frame()
        .expect("next_frame must not error")
        .expect("decoder produced no frame");
    assert_eq!(frame.width(), PW);
    assert_eq!(frame.height(), PH);
}
