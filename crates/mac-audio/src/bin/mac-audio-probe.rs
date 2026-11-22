//! `mac-audio-probe`：ScreenCaptureKit 系统声音 / Opus / AudioQueue 播放真机探针。
//!
//! ```text
//! mac-audio-probe capture [秒数] [输出.wav]   采集系统声音 → Opus 往返 → WAV + 读数
//! mac-audio-probe rt      [秒数]              采集 → 编码 → 抖动缓冲 → AudioQueue 播放（音量 0）
//! mac-audio-probe tone    [秒数]              合成 440 Hz → 编码 → 抖动缓冲 → AudioQueue 播放
//! ```
//!
//! `capture` 需要「屏幕录制」权限（系统设置 → 隐私与安全性 → 屏幕录制）；
//! 第一次运行时系统会弹授权框，授权后需要重启进程。
//!
//! `rt` 把播放音量设为 0：只验证「收包 → 抖动缓冲 → 解码 → 写设备」这条链路，
//! 又不让扬声器的声音被 `capture` 采回来形成正反馈。
//!
//! `tone` 是唯一能**听见**的子命令：它是端到端的合成信号，用来主观确认延迟与连续性。

#[cfg(target_os = "macos")]
fn main() {
    if let Err(err) = run() {
        eprintln!("探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac-audio-probe 仅 macOS 支持：ScreenCaptureKit 与 AudioQueue 都是 macOS 专属框架");
}

#[cfg(target_os = "macos")]
fn run() -> rust_tunnel_mac_audio::MacAudioResult<()> {
    use rust_tunnel_mac_audio::MacAudioError;

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "capture".to_owned());
    let rest: Vec<String> = args.collect();
    match mode.as_str() {
        "capture" => capture(seconds(rest.first(), 3), rest.get(1)),
        "rt" => roundtrip(seconds(rest.first(), 5)),
        "tone" => tone(seconds(rest.first(), 2)),
        other => Err(MacAudioError::Message(format!(
            "未知子命令 {other:?}（capture / rt / tone）"
        ))),
    }
}

#[cfg(target_os = "macos")]
fn seconds(arg: Option<&String>, default: u64) -> u64 {
    arg.and_then(|raw| raw.parse().ok()).unwrap_or(default)
}

/// 采集系统声音 → Opus 往返 → WAV，并打印码率 / 电平读数。
#[cfg(target_os = "macos")]
#[expect(
    clippy::cast_precision_loss,
    reason = "探针只打印近似读数：字节数与采样值都远在 f32 精度上限内"
)]
fn capture(seconds: u64, out: Option<&String>) -> rust_tunnel_mac_audio::MacAudioResult<()> {
    use rust_tunnel_mac_audio::format::{CHANNELS, FRAME_FLOATS, OPUS_BITRATE_BPS, SAMPLE_RATE};
    use rust_tunnel_mac_audio::opus::Decoder;
    use rust_tunnel_mac_audio::{metrics, wav, LoopbackEncoder};

    let encoder = LoopbackEncoder::new()?;
    encoder.start()?;
    println!(
        "已启动 ScreenCaptureKit 系统声音采集（48 kHz 立体声，目标 {OPUS_BITRATE_BPS} bps）。\
         请让系统播放一点声音，采集 {seconds} 秒……"
    );
    let mut decoder = Decoder::new(SAMPLE_RATE, CHANNELS)?;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(seconds);
    let mut pcm_all = Vec::new();
    let mut encoded_bytes = 0_usize;
    let mut packets = 0_u64;
    while std::time::Instant::now() < deadline {
        let Some(packet) = encoder.read_packet(&stop) else {
            continue;
        };
        packets = packets.saturating_add(1);
        encoded_bytes = encoded_bytes.saturating_add(packet.data.len());
        let mut pcm = vec![0.0_f32; FRAME_FLOATS];
        let frames = decoder.decode(&packet.data, &mut pcm, false)?;
        pcm_all.extend_from_slice(&pcm[..frames * usize::from(CHANNELS)]);
    }
    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
    let stats = encoder.stats();
    encoder.stop();

    println!(
        "采集完成：{packets} 帧（{:.2} 秒音频），编码 {} 字节，{:.1} kbps（目标 {} kbps）",
        pcm_all.len() as f64 / f64::from(SAMPLE_RATE * u32::from(CHANNELS)),
        encoded_bytes,
        encoded_bytes as f64 * 8.0 / elapsed / 1000.0,
        OPUS_BITRATE_BPS / 1000
    );
    println!(
        "SCK 回调 {} 次，产出 {} 帧，队列丢帧 {}；RMS {:.4}，峰值 {:.4}{}",
        stats.callbacks,
        stats.frames,
        stats.dropped,
        metrics::rms(&pcm_all),
        metrics::peak(&pcm_all),
        if metrics::peak(&pcm_all) < 0.001 {
            "  ← 全程接近静音：请确认这段时间系统在放声音"
        } else {
            ""
        }
    );
    if let Some(err) = encoder.last_error() {
        println!("内部错误：{err}");
    }
    let path = out
        .cloned()
        .unwrap_or_else(|| "mac-audio-capture.wav".to_owned());
    wav::write_i16(&path, &pcm_all, SAMPLE_RATE, CHANNELS).map_err(|err| {
        rust_tunnel_mac_audio::MacAudioError::Message(format!("写 {path} 失败: {err}"))
    })?;
    println!("已写入 {path}（经 Opus 往返后的采集内容，可直接试听）");
    Ok(())
}

/// 采集 → 编码 → 抖动缓冲 → AudioQueue 播放（音量 0），验证播放链路与统计。
#[cfg(target_os = "macos")]
fn roundtrip(seconds: u64) -> rust_tunnel_mac_audio::MacAudioResult<()> {
    use rust_tunnel_mac_audio::{LoopbackEncoder, Playback};

    let encoder = LoopbackEncoder::new()?;
    encoder.start()?;
    let playback = Playback::new()?;
    playback.start()?;
    // 音量 0：不让扬声器出声，否则系统声音会被 SCK 再采回来形成正反馈。
    playback.set_volume(0);
    println!("回环自检：系统声音 → Opus → 抖动缓冲 → AudioQueue（静音），共 {seconds} 秒……");

    let stop = std::sync::atomic::AtomicBool::new(false);
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(seconds);
    let mut seq = 0_u64;
    while std::time::Instant::now() < deadline {
        let Some(packet) = encoder.read_packet(&stop) else {
            continue;
        };
        seq = seq.saturating_add(1);
        playback.push(seq, packet.data);
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    encoder.stop();
    let stats = playback.stats();
    playback.stop();

    println!(
        "回环自检完成：产出 {seq} 帧，播放端收包 {}，队列丢 {}，迟到 {}，溢出 {}，欠载 {}，PLC {}，目标缓冲 {} ms",
        stats.received,
        stats.dropped,
        stats.late,
        stats.overflow,
        stats.underruns,
        stats.concealed,
        stats.buffer_ms
    );
    if let Some(err) = encoder.last_error() {
        println!("采集侧内部错误：{err}");
    }
    Ok(())
}

/// 合成 440 Hz 正弦 → Opus → 抖动缓冲 → AudioQueue 播放（可听见）。
#[cfg(target_os = "macos")]
#[expect(clippy::cast_precision_loss, reason = "合成正弦只需要 f32 精度")]
fn tone(seconds: u64) -> rust_tunnel_mac_audio::MacAudioResult<()> {
    use rust_tunnel_mac_audio::format::{
        CHANNELS, FRAME_FLOATS, FRAME_FRAMES, FRAME_MS, OPUS_BITRATE_BPS, SAMPLE_RATE,
    };
    use rust_tunnel_mac_audio::opus::{Encoder, MAX_PACKET_BYTES};
    use rust_tunnel_mac_audio::Playback;

    let blocks = usize::try_from(seconds.saturating_mul(1000 / u64::from(FRAME_MS)).max(1))
        .unwrap_or(usize::MAX / 2);
    let mut encoder = Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?;
    let playback = Playback::new()?;
    playback.start()?;
    println!("向默认输出设备播放 {seconds} 秒 440 Hz 正弦（经 Opus + 抖动缓冲）……");

    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    let started = std::time::Instant::now();
    for index in 0..blocks {
        let mut pcm = vec![0.0_f32; FRAME_FLOATS];
        let base = index.saturating_mul(FRAME_FRAMES);
        // 头 20 帧淡入、末 2 帧淡出，避免爆音。
        let fade_in = (index.min(20) as f32 + 1.0) / 21.0;
        let fade_out = if index + 2 >= blocks { 0.5 } else { 1.0 };
        for (i, sample) in pcm.chunks_mut(usize::from(CHANNELS)).enumerate() {
            let frame = base + i;
            let t = frame as f32 / SAMPLE_RATE as f32;
            let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2 * fade_in * fade_out;
            sample.fill(value);
        }
        let written = encoder.encode(&pcm, &mut packet)?;
        playback.push(index as u64 + 1, packet[..written].to_vec());
        let due = std::time::Duration::from_millis(
            u64::from(FRAME_MS).saturating_mul(u64::try_from(index + 1).unwrap_or(u64::MAX)),
        );
        if let Some(wait) = started
            .checked_add(due)
            .and_then(|at| at.checked_duration_since(std::time::Instant::now()))
        {
            std::thread::sleep(wait);
        }
    }
    // 等抖动缓冲与设备缓冲把尾巴播完（80 ms 抖动 + 60 ms 设备缓冲，留足余量）。
    std::thread::sleep(std::time::Duration::from_millis(400));
    let stats = playback.stats();
    playback.stop();
    println!(
        "播放完成：收包 {}，队列丢 {}，欠载 {}，PLC {}，目标缓冲 {} ms",
        stats.received, stats.dropped, stats.underruns, stats.concealed, stats.buffer_ms
    );
    Ok(())
}
