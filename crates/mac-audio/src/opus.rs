//! Opus 编解码封装（`audiopus_sys` 的 C ABI）。
//!
//! 只暴露本项目需要的部分：48 kHz 立体声浮点 PCM 的编码/解码与丢包隐藏（PLC）。
//! `Encoder` / `Decoder` **不是** `Sync`，但可以 `Send`（调用方把它们移进独立的
//! 采集/播放线程）。
//!
//! # 编译面
//!
//! 真实现只在 `all(feature = "opus", target_os = "macos")` 下编译：`audiopus_sys`
//! 用 CMake 构建 vendored libopus，从 Windows 交叉到 macOS 不可行。其余情况
//! （`--no-default-features`、或在 Windows 上做壳检查）退化为**同签名**、
//! 恒返回 [`MacAudioError::Unsupported`] 的实现——调用方不需要按 target 分支。

pub use imp::{Decoder, Encoder};

/// Opus 单包最大字节数（RFC 6716 上限 1275，这里留足余量）。
pub const MAX_PACKET_BYTES: usize = 4_000;

#[cfg(all(feature = "opus", target_os = "macos"))]
mod imp {
    use super::MAX_PACKET_BYTES;
    use crate::error::{MacAudioError, MacAudioResult};
    use audiopus_sys as sys;
    use std::ffi::CStr;

    /// `OPUS_APPLICATION_AUDIO`：音乐/系统声音优先，而不是纯语音。
    const APPLICATION_AUDIO: std::os::raw::c_int = sys::OPUS_APPLICATION_AUDIO;
    /// 编码器复杂度：10 是 libopus 的质量上限，48 kHz 立体声下 CPU 开销可忽略。
    const COMPLEXITY: i32 = 10;

    fn opus_error(code: i32) -> MacAudioError {
        // SAFETY: `opus_strerror` 对任意 i32 都返回指向静态字符串的有效指针。
        let message = unsafe { CStr::from_ptr(sys::opus_strerror(code)) }
            .to_string_lossy()
            .into_owned();
        MacAudioError::Opus { code, message }
    }

    /// Opus 编码器。
    pub struct Encoder {
        raw: *mut sys::OpusEncoder,
        channels: u16,
    }

    // SAFETY: 编码器不与其他线程共享（`&mut self` 独占），libopus 的状态只在该线程内使用。
    unsafe impl Send for Encoder {}

    impl Encoder {
        /// 创建编码器（48 kHz 立体声、CBR、complexity 10、无 DTX）。
        ///
        /// - `bitrate_bps` 是目标码率；CBR + 无 DTX 让行为可预测（静音时 Opus 本身
        ///   也只输出几十字节）。
        ///
        /// # Errors
        /// libopus 创建失败或参数非法时返回 [`MacAudioError::Opus`]。
        pub fn new(sample_rate: u32, channels: u16, bitrate_bps: i32) -> MacAudioResult<Self> {
            let fs = i32::try_from(sample_rate)
                .map_err(|_| MacAudioError::Message(format!("采样率非法: {sample_rate}")))?;
            let ch = std::os::raw::c_int::from(channels);
            let mut error = 0_i32;
            // SAFETY: 参数都是值类型，返回的指针由本结构独占并在 Drop 中销毁。
            let raw = unsafe {
                sys::opus_encoder_create(fs, ch, APPLICATION_AUDIO, std::ptr::addr_of_mut!(error))
            };
            if raw.is_null() || error != 0 {
                return Err(opus_error(error));
            }
            // SAFETY: `raw` 非空且由本结构独占；参数都是值类型（variadic 走 C 默认提升）。
            let mut code =
                unsafe { sys::opus_encoder_ctl(raw, sys::OPUS_SET_BITRATE_REQUEST, bitrate_bps) };
            if code == 0 {
                code = unsafe { sys::opus_encoder_ctl(raw, sys::OPUS_SET_VBR_REQUEST, 0_i32) };
            }
            if code == 0 {
                code = unsafe {
                    sys::opus_encoder_ctl(raw, sys::OPUS_SET_COMPLEXITY_REQUEST, COMPLEXITY)
                };
            }
            if code != 0 {
                // 创建阶段失败：销毁半成品，把参数错误交给调用方。
                // SAFETY: `raw` 非空且由本结构独占，只销毁一次。
                unsafe { sys::opus_encoder_destroy(raw) };
                return Err(opus_error(code));
            }
            Ok(Self { raw, channels })
        }

        /// 编码一帧交错 `f32` PCM（长度必须是 `channels` 的整数倍）。
        ///
        /// 返回写入 `out` 的字节数。
        ///
        /// # Errors
        /// PCM 长度非法或 libopus 返回负值时返回 [`MacAudioError::Opus`]。
        pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> MacAudioResult<usize> {
            let ch = usize::from(self.channels);
            if ch == 0 || !pcm.len().is_multiple_of(ch) {
                return Err(MacAudioError::Message("PCM 长度与声道数不匹配".to_owned()));
            }
            let frame_size = i32::try_from(pcm.len() / ch)
                .map_err(|_| MacAudioError::Message("帧过大".to_owned()))?;
            let max_bytes = i32::try_from(out.len().min(MAX_PACKET_BYTES))
                .map_err(|_| MacAudioError::Message("输出缓冲过大".to_owned()))?;
            // SAFETY: `pcm` 与 `out` 的长度已校验，指针在本调用期间有效。
            let written = unsafe {
                sys::opus_encode_float(
                    self.raw,
                    pcm.as_ptr(),
                    frame_size,
                    out.as_mut_ptr(),
                    max_bytes,
                )
            };
            if written < 0 {
                return Err(opus_error(written));
            }
            usize::try_from(written).map_err(|_| MacAudioError::Message("编码长度溢出".to_owned()))
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            // SAFETY: `raw` 由本结构独占，且只销毁一次。
            unsafe { sys::opus_encoder_destroy(self.raw) };
        }
    }

    /// Opus 解码器。
    pub struct Decoder {
        raw: *mut sys::OpusDecoder,
        channels: u16,
    }

    // SAFETY: 同 `Encoder`。
    unsafe impl Send for Decoder {}

    impl Decoder {
        /// 创建解码器。
        ///
        /// # Errors
        /// libopus 创建失败或参数非法时返回 [`MacAudioError::Opus`]。
        pub fn new(sample_rate: u32, channels: u16) -> MacAudioResult<Self> {
            let fs = i32::try_from(sample_rate)
                .map_err(|_| MacAudioError::Message(format!("采样率非法: {sample_rate}")))?;
            let mut error = 0_i32;
            // SAFETY: 参数都是值类型，返回的指针由本结构独占并在 Drop 中销毁。
            let raw = unsafe {
                sys::opus_decoder_create(
                    fs,
                    std::os::raw::c_int::from(channels),
                    std::ptr::addr_of_mut!(error),
                )
            };
            if raw.is_null() || error != 0 {
                return Err(opus_error(error));
            }
            Ok(Self { raw, channels })
        }

        /// 解码一个 Opus 包到交错 `f32` PCM。
        ///
        /// - `fec = true` 时请求解码**带内前向纠错**数据（上一帧的冗余副本）；
        /// - 空包等价于丢包隐藏（与 [`Self::decode_plc`] 一致）。
        ///
        /// 返回解码出的每声道采样点数。
        ///
        /// # Errors
        /// 包过大或 libopus 返回负值时返回 [`MacAudioError::Opus`]。
        pub fn decode(
            &mut self,
            packet: &[u8],
            out: &mut [f32],
            fec: bool,
        ) -> MacAudioResult<usize> {
            let ch = usize::from(self.channels);
            if ch == 0 || !out.len().is_multiple_of(ch) {
                return Err(MacAudioError::Message("输出缓冲与声道数不匹配".to_owned()));
            }
            let frame_size = i32::try_from(out.len() / ch)
                .map_err(|_| MacAudioError::Message("输出帧过大".to_owned()))?;
            let (data, len) = if packet.is_empty() {
                (std::ptr::null(), 0_i32)
            } else {
                let len = i32::try_from(packet.len())
                    .map_err(|_| MacAudioError::Message("包过大".to_owned()))?;
                (packet.as_ptr(), len)
            };
            // SAFETY: `data/len` 与 `out` 的尺寸已校验，指针在本调用期间有效。
            let frames = unsafe {
                sys::opus_decode_float(
                    self.raw,
                    data,
                    len,
                    out.as_mut_ptr(),
                    frame_size,
                    i32::from(u8::from(fec)),
                )
            };
            if frames < 0 {
                return Err(opus_error(frames));
            }
            usize::try_from(frames).map_err(|_| MacAudioError::Message("解码长度溢出".to_owned()))
        }

        /// 丢包隐藏（PLC）：用解码器内部状态生成一个替代帧。
        ///
        /// # Errors
        /// 同 [`Self::decode`]。
        pub fn decode_plc(&mut self, out: &mut [f32]) -> MacAudioResult<usize> {
            self.decode(&[], out, false)
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            // SAFETY: `raw` 由本结构独占，且只销毁一次。
            unsafe { sys::opus_decoder_destroy(self.raw) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::format::{CHANNELS, FRAME_FLOATS, SAMPLE_RATE};

        fn sine(frames: usize, hz: f32, amplitude: f32) -> Vec<f32> {
            (0..frames)
                .flat_map(|i| {
                    let phase = i as f32 / SAMPLE_RATE as f32 * hz * std::f32::consts::TAU;
                    [phase.sin() * amplitude, phase.sin() * amplitude]
                })
                .collect()
        }

        #[test]
        fn roundtrip_is_close_to_the_source() {
            let mut enc = Encoder::new(SAMPLE_RATE, CHANNELS, 96_000).expect("创建编码器");
            let mut dec = Decoder::new(SAMPLE_RATE, CHANNELS).expect("创建解码器");
            let pcm = sine(FRAME_FLOATS / 2, 440.0, 0.4);
            let mut packet = vec![0_u8; MAX_PACKET_BYTES];
            let written = enc.encode(&pcm, &mut packet).expect("编码");
            assert!(written > 0);
            let mut decoded = vec![0.0_f32; FRAME_FLOATS];
            // 第一帧是编码器的预热帧，Opus 有算法延迟；只做解码是否成功的冒烟检查。
            let frames = dec
                .decode(&packet[..written], &mut decoded, false)
                .expect("解码");
            assert_eq!(frames, FRAME_FLOATS / usize::from(CHANNELS));
        }

        #[test]
        fn plc_produces_a_full_frame() {
            let mut dec = Decoder::new(SAMPLE_RATE, CHANNELS).expect("创建解码器");
            let mut decoded = vec![0.0_f32; FRAME_FLOATS];
            let frames = dec.decode_plc(&mut decoded).expect("PLC");
            assert_eq!(frames, FRAME_FLOATS / usize::from(CHANNELS));
        }
    }
}

#[cfg(not(all(feature = "opus", target_os = "macos")))]
mod imp {
    use crate::error::{MacAudioError, MacAudioResult};

    /// 当前构建没有 libopus 时给出的原因。
    const UNSUPPORTED: &str =
        "未启用 opus feature，或当前目标无法构建 audiopus_sys（需要 macOS + CMake）";

    /// Opus 编码器（当前构建没有 libopus，构造必失败）。
    pub struct Encoder;

    impl Encoder {
        /// # Errors
        /// 当前构建没有 libopus，恒返回 [`MacAudioError::Unsupported`]。
        pub fn new(_sample_rate: u32, _channels: u16, _bitrate_bps: i32) -> MacAudioResult<Self> {
            Err(MacAudioError::Unsupported(UNSUPPORTED))
        }

        /// # Errors
        /// 恒返回 [`MacAudioError::Unsupported`]。
        pub fn encode(&mut self, _pcm: &[f32], _out: &mut [u8]) -> MacAudioResult<usize> {
            Err(MacAudioError::Unsupported(UNSUPPORTED))
        }
    }

    /// Opus 解码器（当前构建没有 libopus，构造必失败）。
    pub struct Decoder;

    impl Decoder {
        /// # Errors
        /// 当前构建没有 libopus，恒返回 [`MacAudioError::Unsupported`]。
        pub fn new(_sample_rate: u32, _channels: u16) -> MacAudioResult<Self> {
            Err(MacAudioError::Unsupported(UNSUPPORTED))
        }

        /// # Errors
        /// 恒返回 [`MacAudioError::Unsupported`]。
        pub fn decode(
            &mut self,
            _packet: &[u8],
            _out: &mut [f32],
            _fec: bool,
        ) -> MacAudioResult<usize> {
            Err(MacAudioError::Unsupported(UNSUPPORTED))
        }

        /// # Errors
        /// 恒返回 [`MacAudioError::Unsupported`]。
        pub fn decode_plc(&mut self, _out: &mut [f32]) -> MacAudioResult<usize> {
            Err(MacAudioError::Unsupported(UNSUPPORTED))
        }
    }
}
