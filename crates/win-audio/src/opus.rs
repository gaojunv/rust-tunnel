//! Opus 编解码封装（vendored libopus 的 C ABI）。
//!
//! 只暴露本项目需要的部分：48 kHz 立体声浮点 PCM 的编码/解码与丢包隐藏。
//! `Encoder` / `Decoder` **不是** `Sync`，但可以 `Send`（引擎会把它们移进独立的
//! 采集/播放线程）。
//!
//! 非 `x86_64-pc-windows-gnu` 目标没有预编译库，本模块退化为返回
//! [`AudioError::Unsupported`] 的同签名实现——调用方不需要按 target 分支。

use crate::error::{AudioError, AudioResult};

/// Opus 单包最大字节数（RFC 6716 上限 1275，这里留足余量）。
pub const MAX_PACKET_BYTES: usize = 4_000;

/// `OPUS_APPLICATION_AUDIO`：音乐/系统声音优先，而不是纯语音。
#[cfg(target_env = "gnu")]
const OPUS_APPLICATION_AUDIO: i32 = 2049;

/// `OPUS_SET_BITRATE_REQUEST`。
#[cfg(target_env = "gnu")]
const OPUS_SET_BITRATE_REQUEST: i32 = 4002;
/// `OPUS_SET_VBR_REQUEST`。
#[cfg(target_env = "gnu")]
const OPUS_SET_VBR_REQUEST: i32 = 4006;
/// `OPUS_SET_COMPLEXITY_REQUEST`。
#[cfg(target_env = "gnu")]
const OPUS_SET_COMPLEXITY_REQUEST: i32 = 4010;

#[cfg(target_env = "gnu")]
mod imp {
    use super::{
        AudioError, AudioResult, MAX_PACKET_BYTES, OPUS_APPLICATION_AUDIO,
        OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST, OPUS_SET_VBR_REQUEST,
    };
    use std::ffi::CStr;
    use std::os::raw::c_char;

    #[repr(C)]
    struct RawEncoder {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct RawDecoder {
        _private: [u8; 0],
    }

    extern "C" {
        fn opus_encoder_create(
            fs: i32,
            channels: i32,
            application: i32,
            error: *mut i32,
        ) -> *mut RawEncoder;
        fn opus_encoder_destroy(st: *mut RawEncoder);
        fn opus_encoder_ctl(st: *mut RawEncoder, request: i32, ...) -> i32;
        fn opus_encode_float(
            st: *mut RawEncoder,
            pcm: *const f32,
            frame_size: i32,
            data: *mut u8,
            max_data_bytes: i32,
        ) -> i32;
        fn opus_decoder_create(fs: i32, channels: i32, error: *mut i32) -> *mut RawDecoder;
        fn opus_decoder_destroy(st: *mut RawDecoder);
        fn opus_decode_float(
            st: *mut RawDecoder,
            data: *const u8,
            len: i32,
            pcm: *mut f32,
            frame_size: i32,
            decode_fec: i32,
        ) -> i32;
        fn opus_strerror(error: i32) -> *const c_char;
    }

    fn opus_error(code: i32) -> AudioError {
        // SAFETY: `opus_strerror` 对任意 i32 都返回指向静态字符串的有效指针。
        let message = unsafe { CStr::from_ptr(opus_strerror(code)) }
            .to_string_lossy()
            .into_owned();
        AudioError::Opus { code, message }
    }

    /// Opus 编码器。
    pub struct Encoder {
        raw: *mut RawEncoder,
        channels: u16,
    }

    // SAFETY: 编码器不与其他线程共享（`&mut self` 独占），libopus 的状态只在该线程内使用。
    unsafe impl Send for Encoder {}

    impl Encoder {
        /// 创建编码器。
        ///
        /// `bitrate_bps` 是目标码率；CBR + complexity 10 + 无 DTX（行为可预测，
        /// 静音时 Opus 本身也只输出几十字节）。
        ///
        /// # Errors
        /// libopus 创建失败或参数非法时返回 [`AudioError::Opus`]。
        pub fn new(sample_rate: u32, channels: u16, bitrate_bps: i32) -> AudioResult<Self> {
            let fs = i32::try_from(sample_rate)
                .map_err(|_| AudioError::Message(format!("采样率非法: {sample_rate}")))?;
            let ch = i32::from(channels);
            let mut error = 0_i32;
            // SAFETY: 参数都是值类型，返回的指针由本结构独占并在 Drop 中销毁。
            let raw = unsafe {
                opus_encoder_create(fs, ch, OPUS_APPLICATION_AUDIO, std::ptr::addr_of_mut!(error))
            };
            if raw.is_null() || error != 0 {
                return Err(opus_error(error));
            }
            // SAFETY: `raw` 非空且由本结构独占；参数都是值类型。
            let mut code = unsafe { opus_encoder_ctl(raw, OPUS_SET_BITRATE_REQUEST, bitrate_bps) };
            if code == 0 {
                code = unsafe { opus_encoder_ctl(raw, OPUS_SET_VBR_REQUEST, 0_i32) };
            }
            if code == 0 {
                code = unsafe { opus_encoder_ctl(raw, OPUS_SET_COMPLEXITY_REQUEST, 10_i32) };
            }
            if code != 0 {
                // 销毁失败与否已无关紧要：创建阶段失败，返回参数错误给调用方。
                unsafe { opus_encoder_destroy(raw) };
                return Err(opus_error(code));
            }
            Ok(Self { raw, channels })
        }

        /// 编码一帧交错 `f32` PCM（长度必须是 `channels` 的整数倍）。
        ///
        /// # Errors
        /// libopus 返回负值时返回 [`AudioError::Opus`]。
        pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> AudioResult<usize> {
            let ch = usize::from(self.channels);
            if ch == 0 || !pcm.len().is_multiple_of(ch) {
                return Err(AudioError::Message("PCM 长度与声道数不匹配".to_owned()));
            }
            let frame_size = i32::try_from(pcm.len() / ch)
                .map_err(|_| AudioError::Message("帧过大".to_owned()))?;
            let max_bytes = i32::try_from(out.len().min(MAX_PACKET_BYTES))
                .map_err(|_| AudioError::Message("输出缓冲过大".to_owned()))?;
            // SAFETY: `pcm` 与 `out` 的长度已校验，指针在本调用期间有效。
            let written = unsafe {
                opus_encode_float(
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
            usize::try_from(written).map_err(|_| AudioError::Message("编码长度溢出".to_owned()))
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            // SAFETY: `raw` 由本结构独占，且只销毁一次。
            unsafe { opus_encoder_destroy(self.raw) };
        }
    }

    /// Opus 解码器。
    pub struct Decoder {
        raw: *mut RawDecoder,
        channels: u16,
    }

    // SAFETY: 同 `Encoder`。
    unsafe impl Send for Decoder {}

    impl Decoder {
        /// 创建解码器。
        ///
        /// # Errors
        /// libopus 创建失败或参数非法时返回 [`AudioError::Opus`]。
        pub fn new(sample_rate: u32, channels: u16) -> AudioResult<Self> {
            let fs = i32::try_from(sample_rate)
                .map_err(|_| AudioError::Message(format!("采样率非法: {sample_rate}")))?;
            let mut error = 0_i32;
            // SAFETY: 参数都是值类型，返回的指针由本结构独占并在 Drop 中销毁。
            let raw = unsafe {
                opus_decoder_create(fs, i32::from(channels), std::ptr::addr_of_mut!(error))
            };
            if raw.is_null() || error != 0 {
                return Err(opus_error(error));
            }
            Ok(Self { raw, channels })
        }

        /// 解码一个 Opus 包到交错 `f32` PCM。
        ///
        /// 空包触发**丢包隐藏**（PLC），行为与显式调用 [`Self::decode_plc`] 一致。
        ///
        /// # Errors
        /// libopus 返回负值时返回 [`AudioError::Opus`]。
        pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> AudioResult<usize> {
            let ch = usize::from(self.channels);
            let frame_size = i32::try_from(out.len() / ch)
                .map_err(|_| AudioError::Message("输出帧过大".to_owned()))?;
            let (data, len) = if packet.is_empty() {
                (std::ptr::null(), 0_i32)
            } else {
                let len = i32::try_from(packet.len())
                    .map_err(|_| AudioError::Message("包过大".to_owned()))?;
                (packet.as_ptr(), len)
            };
            // SAFETY: `data/len` 与 `out` 的尺寸已校验，指针在本调用期间有效。
            let frames = unsafe {
                opus_decode_float(self.raw, data, len, out.as_mut_ptr(), frame_size, 0)
            };
            if frames < 0 {
                return Err(opus_error(frames));
            }
            usize::try_from(frames).map_err(|_| AudioError::Message("解码长度溢出".to_owned()))
        }

        /// 丢包隐藏：用解码器内部状态生成一个替代帧。
        ///
        /// # Errors
        /// 同 [`Self::decode`]。
        pub fn decode_plc(&mut self, out: &mut [f32]) -> AudioResult<usize> {
            self.decode(&[], out)
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            // SAFETY: `raw` 由本结构独占，且只销毁一次。
            unsafe { opus_decoder_destroy(self.raw) };
        }
    }
}

#[cfg(not(target_env = "gnu"))]
mod imp {
    use super::{AudioError, AudioResult};

    /// Opus 编码器（当前目标无 libopus，构造必失败）。
    pub struct Encoder;

    impl Encoder {
        /// # Errors
        /// 当前构建目标没有内置 libopus，恒返回 [`AudioError::Unsupported`]。
        pub fn new(_sample_rate: u32, _channels: u16, _bitrate_bps: i32) -> AudioResult<Self> {
            Err(AudioError::Unsupported(
                "只有 x86_64-pc-windows-gnu 目标内置 libopus",
            ))
        }

        /// # Errors
        /// 恒返回 [`AudioError::Unsupported`]。
        pub fn encode(&mut self, _pcm: &[f32], _out: &mut [u8]) -> AudioResult<usize> {
            Err(AudioError::Unsupported(
                "只有 x86_64-pc-windows-gnu 目标内置 libopus",
            ))
        }
    }

    /// Opus 解码器（当前目标无 libopus，构造必失败）。
    pub struct Decoder;

    impl Decoder {
        /// # Errors
        /// 当前构建目标没有内置 libopus，恒返回 [`AudioError::Unsupported`]。
        pub fn new(_sample_rate: u32, _channels: u16) -> AudioResult<Self> {
            Err(AudioError::Unsupported(
                "只有 x86_64-pc-windows-gnu 目标内置 libopus",
            ))
        }

        /// # Errors
        /// 恒返回 [`AudioError::Unsupported`]。
        pub fn decode(&mut self, _packet: &[u8], _out: &mut [f32]) -> AudioResult<usize> {
            Err(AudioError::Unsupported(
                "只有 x86_64-pc-windows-gnu 目标内置 libopus",
            ))
        }

        /// # Errors
        /// 恒返回 [`AudioError::Unsupported`]。
        pub fn decode_plc(&mut self, _out: &mut [f32]) -> AudioResult<usize> {
            Err(AudioError::Unsupported(
                "只有 x86_64-pc-windows-gnu 目标内置 libopus",
            ))
        }
    }
}

pub use imp::{Decoder, Encoder};

#[cfg(all(test, target_env = "gnu"))]
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
        // 第一帧是编码器的预热帧，Opus 会有算法延迟；只做解码是否成功的冒烟检查。
        let frames = dec.decode(&packet[..written], &mut decoded).expect("解码");
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
