//! Loopback PTY service: exposes interactive shells over a local TCP port so the
//! server can reach them via the existing `OpenTunnel` byte stream.

use std::io::{Read, Write};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// PTY 服务固定回环端口。用固定端口而非随机端口，是为了让服务端在不新增协议
/// 消息的前提下，仅凭 `open_tunnel(client_name, "127.0.0.1:45631")` 就能直连；
/// 端口被占用时 listen 失败，调用方只 warn 不退出（服务端会按版本门控降级）。
pub const DEFAULT_PTY_PORT: u16 = 45631;

/// 协商帧首行最大长度：4KB 足够容纳 rows/cols/shell，超限直接断开防畸形请求
const MAX_NEGOTIATION_BYTES: usize = 4 * 1024;

/// 首行 JSON 协商帧：`{"rows":24,"cols":80,"shell":"可选"}`。
/// rows/cols 缺省时取交互终端常见尺寸（serde default）；shell 为 None 时用系统
/// 默认 shell。服务端会复用同一帧把 docker exec 的整串命令放在 shell 字段。
#[derive(Debug, Deserialize)]
struct Negotiation {
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default)]
    shell: Option<String>,
}

fn default_rows() -> u16 {
    24
}

fn default_cols() -> u16 {
    80
}

impl Negotiation {
    /// 钳制 rows/cols 到合理范围（1..=500），防止畸形帧导致超大 PTY 分配
    fn clamp_size(&mut self) {
        self.rows = self.rows.clamp(1, 500);
        self.cols = self.cols.clamp(1, 500);
    }
}

/// 在回环地址上启动 PTY 服务（绑定 + accept 循环一次完成）。
/// listen 失败由调用方决定：客户端入口只 warn 不退出，服务端按客户端版本门控降级。
///
/// # Errors
/// 端口被占用或底层系统调用失败时返回 `std::io::Error`。
pub async fn serve(port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("agent PTY service listening on 127.0.0.1:{port}");
    serve_on(listener).await
}

/// 对已绑定的 listener 运行 accept 循环。bind 与 serve 分离是为了测试可以用
/// `127.0.0.1:0` 拿到动态端口。单个连接的失败只关闭该连接，不影响整体服务。
///
/// # Errors
/// 此循环通常不返回；仅当 accept 持续失败等无法恢复的情况才会以错误结束。
pub async fn serve_on(listener: TcpListener) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            // accept 偶发错误（如连接被客户端重置）不值得终止服务
            Err(e) => {
                tracing::warn!("PTY accept failed: {e}");
                continue;
            }
        };
        tracing::debug!("PTY connection from {peer}");
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                tracing::warn!("PTY connection from {peer} ended: {e}");
            }
        });
    }
}

/// 单个 PTY 连接的生命周期：协商 → 起 shell → 双向桥接 → 确保 kill/回收子进程。
async fn handle_connection(stream: TcpStream) -> std::io::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut buf_reader = BufReader::new(read_half);

    let mut negotiation = read_negotiation(&mut buf_reader).await?;
    negotiation.clamp_size();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: negotiation.rows,
            cols: negotiation.cols,
            // pixel 尺寸未知，传 0 让 PTY 只按 rows/cols 布局
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(std::io::Error::other)?;

    let cmd = build_shell_command(negotiation.shell.as_deref());
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(std::io::Error::other)?;
    // kill 需要跨任务共享：clone 一份给 TCP→PTY 方向，主路径保留 child 本体兜底
    let tcp_killer = child.clone_killer();
    let mut master_writer = pair.master.take_writer().map_err(std::io::Error::other)?;
    let master_reader = pair
        .master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;

    // PTY 读 → mpsc → TCP 写。master reader 是 blocking fd，放进 spawn_blocking；
    // 有界通道（32）把背压传导给读循环，避免 shell 高频输出撑爆内存。
    // tx 本体保留在主路径用于主动 drop 关闭通道；reader 任务用 clone。
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    let tx_reader = tx.clone();
    let reader_task = tokio::task::spawn_blocking(move || pty_read_loop(master_reader, &tx_reader));

    // TCP → PTY：tokio 读 half → std writer。PTY master 写是 blocking fd 但写
    // 入缓冲很小，不会长期阻塞；短写由 Write::write_all 的循环兜底。
    let tcp_to_pty = tokio::spawn(async move {
        let mut tcp_killer = tcp_killer;
        let mut buf = vec![0u8; 8192];
        loop {
            match buf_reader.read(&mut buf).await {
                // TCP 端关闭：kill shell，PTY reader 随之 EOF，另一端自然收尾
                Ok(0) | Err(_) => {
                    let _ = tcp_killer.kill();
                    break;
                }
                Ok(n) => {
                    if let Err(e) = master_writer.write_all(&buf[..n]) {
                        tracing::warn!("PTY write failed: {e}");
                        let _ = tcp_killer.kill();
                        break;
                    }
                }
            }
        }
    });

    // 通道 → TCP 写 half
    let pty_to_tcp = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(chunk) = rx.recv().await {
            if write_half.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // 子进程退出或对端关闭：发 FIN 让服务端感知隧道结束
        let _ = write_half.shutdown().await;
    });

    // 主路径：等 TCP → PTY 方向结束（客户端断开 / 写 PTY 失败）即完成一轮会话
    let _ = tcp_to_pty.await;

    // 所有分支结束时子进程都必须被终止（kill_on_drop 语义），并回收避免僵尸进程。
    // 已自然退出的进程 kill 会返回错误，忽略即可。
    let _ = child.kill();
    drop(tx); // 关闭通道，让 pty_to_tcp 立即收尾
    let mut reap_child = child;
    tokio::task::spawn_blocking(move || {
        let _ = reap_child.wait();
    })
    .await
    .ok();

    // child 已死 → master reader EOF → reader 任务结束；写任务因通道关闭而结束。
    // 带超时兜底，防极端情况（kill 后子进程仍占着 slave fd）导致任务悬挂。
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), reader_task).await;
    let _ = pty_to_tcp.await;
    Ok(())
}

/// 读首行协商帧并解析。行长度受 `MAX_NEGOTIATION_BYTES` 限制：这里用 `fill_buf`
/// 手动消费而不是 `read_line`，因为 `read_line` 会无限增长缓冲直到遇到换行，恶意
/// 对端可以借此耗尽客户端内存。
async fn read_negotiation(
    reader: &mut BufReader<impl AsyncRead + Unpin>,
) -> std::io::Result<Negotiation> {
    let mut line = Vec::with_capacity(128);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "closed before negotiation",
                ));
            }
            break; // EOF 前已有数据，视为完整一行
        }
        if let Some(idx) = available.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&available[..idx]);
            reader.consume(idx + 1); // 消费换行符
            break;
        }
        // 没有换行：整块缓冲并入行。先取长度再 consume——consume 需要 &mut reader，
        // 而 available 借用 reader，必须在其最后一次使用之后才能再次可变借用。
        let n = available.len();
        line.extend_from_slice(available);
        reader.consume(n);
        if line.len() > MAX_NEGOTIATION_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negotiation line exceeds 4KB",
            ));
        }
    }
    let text = String::from_utf8_lossy(&line);
    serde_json::from_str(text.as_ref()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid negotiation JSON: {e}"),
        )
    })
}

/// 构造 shell 命令。
/// - `shell` 为 Some 时以 `sh -c '<shell>'` 执行：服务端把 docker exec 场景的整串
///   命令（如 `docker exec -it <ctr> sh`）放进该字段；
/// - None 时用系统默认交互 shell（Unix：$SHELL 或 /bin/sh；Windows：cmd.exe）。
fn build_shell_command(shell: Option<&str>) -> CommandBuilder {
    #[cfg(not(windows))]
    {
        if let Some(cmd) = shell {
            let mut builder = CommandBuilder::new("sh");
            builder.args(["-c", cmd]);
            builder
        } else {
            // $SHELL 是用户偏好的交互 shell；缺失时回退 POSIX sh
            let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            CommandBuilder::new(program)
        }
    }
    #[cfg(windows)]
    {
        // Windows 无 POSIX sh：统一走 cmd.exe（portable-pty 内部使用 ConPTY）
        let mut builder = CommandBuilder::new("cmd.exe");
        if let Some(cmd) = shell {
            builder.args(["/C", cmd]);
        }
        builder
    }
}

/// blocking 读循环：把 PTY master 输出搬进 mpsc 通道，运行在 `spawn_blocking` 线程。
/// 子进程退出或 PTY 关闭时读到 EOF/EIO（Linux 下 slave 全部关闭时 master 读返回
/// EIO），退出循环并 drop tx；通道随之关闭，下游写任务据此发 FIN。
fn pty_read_loop(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // 对端已关闭（rx drop）→ 退出读循环
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_parses() {
        let n: Negotiation = serde_json::from_str(r#"{"rows":24,"cols":80}"#).unwrap();
        assert_eq!(n.rows, 24);
        assert_eq!(n.cols, 80);
        assert!(n.shell.is_none());
    }

    #[test]
    fn negotiation_missing_fields_use_defaults() {
        let n: Negotiation = serde_json::from_str("{}").unwrap();
        assert_eq!(n.rows, 24);
        assert_eq!(n.cols, 80);
        assert!(n.shell.is_none());
    }

    #[test]
    fn negotiation_clamps_extremes() {
        let mut n: Negotiation =
            serde_json::from_str(r#"{"rows":0,"cols":5000,"shell":"bash"}"#).unwrap();
        n.clamp_size();
        assert_eq!(n.rows, 1);
        assert_eq!(n.cols, 500);
        assert_eq!(n.shell.as_deref(), Some("bash"));
    }

    #[test]
    fn negotiation_rejects_non_json() {
        assert!(serde_json::from_str::<Negotiation>("not json").is_err());
        // 字段类型不匹配也拒绝（"x" 无法解析为 u16）
        assert!(serde_json::from_str::<Negotiation>(r#"{"rows":"x"}"#).is_err());
    }

    #[tokio::test]
    async fn negotiation_line_too_long_rejected() {
        let line = format!("{}\n", "x".repeat(MAX_NEGOTIATION_BYTES + 1));
        let mut reader = BufReader::new(line.as_bytes());
        let err = read_negotiation(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn negotiation_reads_and_parses() {
        let mut reader = BufReader::new(br#"{"rows":40,"cols":120,"shell":"bash"}"#.as_slice());
        let n = read_negotiation(&mut reader).await.unwrap();
        assert_eq!(n.rows, 40);
        assert_eq!(n.cols, 120);
        assert_eq!(n.shell.as_deref(), Some("bash"));
    }

    /// 集成冒烟：真实 shell 通过协商帧运行 `echo hello-pty`，输出应能回到 TCP 对端。
    /// Unix-only：CI 是 Linux musl；Windows 的 `ConPTY` 分支代码只写不验证。
    #[cfg(unix)]
    #[tokio::test]
    async fn pty_echo_smoke() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_on(listener));

        let mut stream = TcpStream::connect(addr).await.unwrap();
        // 协商帧必须以换行结尾（服务端按行读取）
        stream
            .write_all(br#"{"rows":24,"cols":80,"shell":"echo hello-pty"}"#)
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();

        // 读输出直到命中目标或超时；shell 退出后服务端发 FIN，read 返回 0
        let mut output = Vec::new();
        let mut buf = [0u8; 1024];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !output.windows(9).any(|w| w == b"hello-pty") && std::time::Instant::now() < deadline
        {
            match tokio::time::timeout(std::time::Duration::from_millis(200), stream.read(&mut buf))
                .await
            {
                Ok(Ok(0) | Err(_)) => break, // EOF/错误：服务端已关闭连接
                Err(_) => {}                 // 单次超时：循环继续等到 deadline
                Ok(Ok(n)) => output.extend_from_slice(&buf[..n]),
            }
        }

        drop(stream); // 关闭让服务端 tcp_to_pty 任务正常收尾
        server.abort();

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-pty"),
            "pty output did not contain marker, got: {text:?}"
        );
    }
}
