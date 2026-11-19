//! `win-audio-probe`：WASAPI 设备 / loopback 采集 / Opus 往返 / 播放真机探针。
//!
//! ```text
//! win-audio-probe devices
//! win-audio-probe capture [秒数] [输出.wav]
//! win-audio-probe rt      [秒数] [输出.wav]
//! win-audio-probe tone    [秒数]
//! win-audio-probe playtest [秒数]
//! ```

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-audio-probe 只能在 Windows 上运行");
}

#[cfg(windows)]
fn run() -> rust_tunnel_win_audio::AudioResult<()> {
    use rust_tunnel_win_audio::AudioError;
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "devices".to_owned());
    // 剩余参数按位置取：秒数、输出路径。
    let rest: Vec<String> = args.collect();

    match mode.as_str() {
        "devices" => list_devices(),
        "capture" => capture(seconds(rest.first(), 3), rest.get(1)),
        "rt" => roundtrip(seconds(rest.first(), 5), rest.get(1)),
        "tone" => tone(seconds(rest.first(), 2)),
        "playtest" => playtest(seconds(rest.first(), 2)),
        other => Err(AudioError::Message(format!(
            "未知子命令 {other:?}（devices / capture / rt / tone）"
        ))),
    }
}

#[cfg(windows)]
fn seconds(arg: Option<&String>, default: u64) -> u64 {
    arg.and_then(|raw| raw.parse().ok()).unwrap_or(default)
}

#[cfg(windows)]
fn list_devices() -> rust_tunnel_win_audio::AudioResult<()> {
    let devices = rust_tunnel_win_audio::wasapi::list_render_devices()?;
    println!("活动回放设备 {} 个：", devices.len());
    for (index, device) in devices.iter().enumerate() {
        println!(
            "  [{index}] {}{}",
            if device.is_default { "* " } else { "  " },
            device.id
        );
    }
    if devices.is_empty() {
        println!("（没有活动的回放设备——无人值守实例在无音频端点的会话里会出现这种情况）");
    }
    Ok(())
}

#[cfg(windows)]
#[expect(
    clippy::cast_precision_loss,
    reason = "探针只打印近似读数：采样数与采样值都远在 f32 精度上限内"
)]
fn capture(
    seconds: u64,
    out: Option<&String>,
) -> rust_tunnel_win_audio::AudioResult<()> {
    use rust_tunnel_win_audio::format::{CHANNELS, FRAME_MS, SAMPLE_RATE};
    use rust_tunnel_win_audio::wav;
    use rust_tunnel_win_audio::{metrics, Capture};

    let blocks = blocks_for(seconds, FRAME_MS);
    let mut source = Capture::open_default_loopback()?;
    println!(
        "已打开默认回放设备的 loopback（缓冲 {} 帧）。开始采集 {seconds} 秒……",
        source.buffer_frames()
    );
    let mut all = Vec::with_capacity(blocks * 1920);
    let mut block = Vec::new();
    for _ in 0..blocks {
        if !source.read_block(&mut block, std::time::Duration::from_millis(500))? {
            eprintln!("采集超时（500 ms 没有攒够一帧），提前结束");
            break;
        }
        all.extend_from_slice(&block);
    }
    println!(
        "采集完成：{} 采样点（{:.2} 秒），RMS {:.4}，峰值 {:.4}{}",
        all.len(),
        all.len() as f64 / f64::from(SAMPLE_RATE * u32::from(CHANNELS)),
        metrics::rms(&all),
        metrics::peak(&all),
        if metrics::peak(&all) < 0.001 {
            "  ← 全程接近静音：请确认这段时间系统在放声音"
        } else {
            ""
        }
    );
    let path = out.map_or_else(|| "audio-capture.wav".to_owned(), Clone::clone);
    wav::write_i16(&path, &all, SAMPLE_RATE, CHANNELS).map_err(|err| {
        rust_tunnel_win_audio::AudioError::Message(format!("写 {path} 失败: {err}"))
    })?;
    println!("已写入 {path}");
    Ok(())
}

#[cfg(windows)]
#[expect(
    clippy::cast_precision_loss,
    reason = "探针只打印近似读数：字节数与采样值都远在 f32 精度上限内"
)]
fn roundtrip(
    seconds: u64,
    out: Option<&String>,
) -> rust_tunnel_win_audio::AudioResult<()> {
    use rust_tunnel_win_audio::format::{
        CHANNELS, FRAME_FLOATS, FRAME_MS, OPUS_BITRATE_BPS, SAMPLE_RATE,
    };
    use rust_tunnel_win_audio::opus::{Decoder, Encoder, MAX_PACKET_BYTES};
    use rust_tunnel_win_audio::{metrics, wav, Capture};

    let blocks = blocks_for(seconds, FRAME_MS);
    let mut source = Capture::open_default_loopback()?;
    println!("采集 {seconds} 秒并做 Opus {OPUS_BITRATE_BPS} bps 往返……");
    let mut encoder = Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?;
    let mut decoder = Decoder::new(SAMPLE_RATE, CHANNELS)?;
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    let mut captured_pcm = Vec::with_capacity(blocks * FRAME_FLOATS);
    let mut roundtrip_pcm = Vec::with_capacity(blocks * FRAME_FLOATS);
    let mut encoded_bytes = 0_usize;
    let mut block = Vec::new();
    let started = std::time::Instant::now();
    for _ in 0..blocks {
        if !source.read_block(&mut block, std::time::Duration::from_millis(500))? {
            eprintln!("采集超时，提前结束");
            break;
        }
        captured_pcm.extend_from_slice(&block);
        let written = encoder.encode(&block, &mut packet)?;
        encoded_bytes += written;
        let mut pcm = vec![0.0_f32; FRAME_FLOATS];
        let frames = decoder.decode(&packet[..written], &mut pcm)?;
        roundtrip_pcm.extend_from_slice(&pcm[..frames * usize::from(CHANNELS)]);
    }
    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
    let n = roundtrip_pcm.len().min(captured_pcm.len());
    println!(
        "编码 {} 字节，{:.1} kbps（目标 {} kbps）",
        encoded_bytes,
        encoded_bytes as f64 * 8.0 / elapsed / 1000.0,
        OPUS_BITRATE_BPS / 1000
    );
    println!(
        "源 RMS {:.4} / 解码 RMS {:.4}；对齐 MAD {:.6}（Opus 有算法延迟，MAD 会偏高）",
        metrics::rms(&captured_pcm[..n]),
        metrics::rms(&roundtrip_pcm[..n]),
        // 延迟未对齐，MAD 只作为「解码器工作正常」的粗读数；精确对齐留给播放侧抖动缓冲。
        metrics::mad(&captured_pcm[..n], &roundtrip_pcm[..n]),
    );
    let path = out.map_or_else(|| "audio-roundtrip.wav".to_owned(), Clone::clone);
    wav::write_i16(&path, &roundtrip_pcm, SAMPLE_RATE, CHANNELS).map_err(|err| {
        rust_tunnel_win_audio::AudioError::Message(format!("写 {path} 失败: {err}"))
    })?;
    println!("已写入 {path}（可试听往返结果）");
    Ok(())
}

#[cfg(windows)]
fn tone(seconds: u64) -> rust_tunnel_win_audio::AudioResult<()> {
    use rust_tunnel_win_audio::format::{
        CHANNELS, FRAME_FLOATS, FRAME_MS, OPUS_BITRATE_BPS, SAMPLE_RATE,
    };
    use rust_tunnel_win_audio::opus::{Decoder, Encoder, MAX_PACKET_BYTES};
    use rust_tunnel_win_audio::Render;

    let blocks = blocks_for(seconds, FRAME_MS);
    let mut encoder = Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?;
    let mut decoder = Decoder::new(SAMPLE_RATE, CHANNELS)?;
    let mut render = Render::open_default_output()?;
    println!("向默认输出设备播放 {seconds} 秒 440 Hz 正弦（经 Opus 往返）……");
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    for index in 0..blocks {
        let pcm = sine_block(index, blocks);
        let written = encoder.encode(&pcm, &mut packet)?;
        let mut decoded_pcm = vec![0.0_f32; FRAME_FLOATS];
        let frames = decoder.decode(&packet[..written], &mut decoded_pcm)?;
        render.write_block(&decoded_pcm[..frames * usize::from(CHANNELS)])?;
    }
    println!("播放完成。");
    Ok(())
}

#[cfg(windows)]
fn playtest(seconds: u64) -> rust_tunnel_win_audio::AudioResult<()> {
    use rust_tunnel_win_audio::format::{CHANNELS, FRAME_MS, OPUS_BITRATE_BPS, SAMPLE_RATE};
    use rust_tunnel_win_audio::opus::{Encoder, MAX_PACKET_BYTES};
    use rust_tunnel_win_audio::playback::Playback;

    let blocks = blocks_for(seconds, FRAME_MS);
    let mut encoder = Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?;
    let playback = Playback::start()?;
    println!("播放自检：合成 440 Hz → Opus → 抖动缓冲 → 默认输出，共 {seconds} 秒……");
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    let mut seq = 0_u64;
    let started = std::time::Instant::now();
    for index in 0..blocks {
        let pcm = sine_block(index, blocks);
        let written = encoder.encode(&pcm, &mut packet)?;
        seq = seq.wrapping_add(1);
        playback.push(seq, packet[..written].to_vec());
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
    // 等抖动缓冲与设备缓冲把尾巴播完（80 ms + 200 ms 缓冲，留足余量）。
    std::thread::sleep(std::time::Duration::from_millis(500));
    let stats = playback.snapshot();
    println!(
        "播放自检完成：收包 {}，队列丢包 {}，欠载 {}，目标缓冲 {} ms",
        stats.received, stats.dropped, stats.underruns, stats.buffer_ms
    );
    Ok(())
}

#[cfg(windows)]
#[expect(
    clippy::cast_precision_loss,
    reason = "合成正弦只需要 f32 精度"
)]
fn sine_block(index: usize, total: usize) -> Vec<f32> {
    use rust_tunnel_win_audio::format::{CHANNELS, FRAME_FRAMES, SAMPLE_RATE};

    let mut pcm = vec![0.0_f32; FRAME_FRAMES * usize::from(CHANNELS)];
    let base = index.saturating_mul(FRAME_FRAMES);
    for (i, sample) in pcm.chunks_mut(usize::from(CHANNELS)).enumerate() {
        let frame = base + i;
        let t = frame as f32 / SAMPLE_RATE as f32;
        // 头 20 帧淡入，末 2 帧淡出，避免爆音。
        let fade_in = (index.min(20) as f32 + 1.0) / 21.0;
        let fade_out = if index + 2 >= total { 0.5 } else { 1.0 };
        let value =
            (t * 440.0 * std::f32::consts::TAU).sin() * 0.2 * fade_in * fade_out;
        sample.fill(value);
    }
    pcm
}

#[cfg(windows)]
fn blocks_for(seconds: u64, frame_ms: u32) -> usize {
    // 向上取整，至少一帧。
    let per_second = 1000 / u64::from(frame_ms.max(1));
    usize::try_from((seconds * per_second).max(1)).unwrap_or(usize::MAX / 2)
}
