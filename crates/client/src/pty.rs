//! Loopback PTY service: exposes interactive shells over a local TCP port so the
//! server can reach them via the existing `OpenTunnel` byte stream.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::LazyLock;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

/// PTY 服务固定回环端口。用固定端口而非随机端口，是为了让服务端在不新增协议
/// 消息的前提下，仅凭 `open_tunnel(client_name, "127.0.0.1:45631")` 就能直连；
/// 端口被占用时 listen 失败，调用方只 warn 不退出（服务端会按版本门控降级）。
pub use rust_tunnel_common::pty::DEFAULT_PTY_PORT;

/// 协商帧首行最大长度：4KB 足够容纳 rows/cols/shell/id/resize_for，超限直接断开防畸形请求
const MAX_NEGOTIATION_BYTES: usize = 4 * 1024;

/// 全局 PTY 会话注册表：`id → resize 通道`。
/// resize_for 协商帧到达时按 id 查找对应通道发送 (rows, cols)；
/// 正常会话建立时注册，连接结束时移除。
static PTY_REGISTRY: LazyLock<Mutex<HashMap<String, mpsc::Sender<(u16, u16)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 首行 JSON 协商帧：`{"rows":24,"cols":80,"shell":"可选","id":"可选","resize_for":"可选"}`。
/// rows/cols 缺省时取交互终端常见尺寸（serde default）；shell 为 None 时用系统
/// 默认 shell。服务端会复用同一帧把 docker exec 的整串命令放在 shell 字段。
///
/// `id` 为终端会话唯一标识（UUID v4），由服务端生成；客户端建立正常会话时注册
/// 到全局 PTY_REGISTRY，供后续 resize_for 协商帧定位。
///
/// `resize_for` 为 resize 重协商目标 id：携带此字段时客户端查找对应 PTY 实例
/// 发送 resize 信号后正常关闭本连接（不建立新 shell）。
#[derive(Debug, Deserialize)]
struct Negotiation {
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default)]
    shell: Option<String>,
    /// 终端会话唯一标识（服务端生成 UUID v4）。
    #[serde(default)]
    id: Option<String>,
    /// resize 重协商目标 id：非 None 时查找已有 PTY 实例发送 resize 后关闭。
    #[serde(default)]
    resize_for: Option<String>,
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
///
/// `resize_for` 协商帧到达时：查找全局 PTY_REGISTRY 发送 resize 信号后立即返回
/// （不建立新 shell）。正常会话：有 `id` 时注册到 PTY_REGISTRY，连接结束移除。
/// resize 通道的接收端在 TCP→PTY 任务中以非阻塞方式轮询（`try_recv`），确保
/// resize 不阻塞数据转发。
async fn handle_connection(stream: TcpStream) -> std::io::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut buf_reader = BufReader::new(read_half);

    let mut negotiation = read_negotiation(&mut buf_reader).await?;
    negotiation.clamp_size();

    // resize_for 协商帧：查找已有 PTY 实例发送 resize 信号后关闭。
    if let Some(ref target_id) = negotiation.resize_for {
        let registry = PTY_REGISTRY.lock().await;
        if let Some(tx) = registry.get(target_id) {
            // 发送 resize 信号（有界通道，满时丢弃——高频 resize 不阻塞）
            let _ = tx.try_send((negotiation.rows, negotiation.cols));
            tracing::debug!(
                target_id,
                rows = negotiation.rows,
                cols = negotiation.cols,
                "resize signal sent"
            );
        } else {
            tracing::debug!(target_id, "resize_for: PTY session not found, ignored");
        }
        // resize 连接使命完成，正常关闭
        return Ok(());
    }

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

    // resize 通道：resize_tx 注册到全局 PTY_REGISTRY，resize_rx 由 TCP→PTY
    // 任务在每次写入 PTY 后 non-blocking poll（try_recv），避免阻塞数据转发。
    // `Box<dyn MasterPty>` 不实现 Clone，无法跨任务共享 master；resize 调用
    // 直接在 TCP→PTY 内联处理，无需独立任务。
    let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>(8);

    // 注册到全局 PTY_REGISTRY（有 id 时）；连接结束移除。
    let registry_id = negotiation.id.clone();
    if let Some(ref id) = registry_id {
        PTY_REGISTRY.lock().await.insert(id.clone(), resize_tx);
    }

    // 提取 master：slave 已被 spawn_command 消费（partially moved），master 须在
    // async block 之前移出 pair，避免 "use of partially moved struct" 编译错误。
    // master move 进 TCP→PTY async block 后保持存活；take_writer 不消费 master，
    // resize() 为 &self 方法——两者在同一 async block 内安全共存。
    let pty_master = pair.master;
    let master_reader = pty_master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;

    // PTY 读 → mpsc → TCP 写。master reader 是 blocking fd，放进 spawn_blocking；
    // 有界通道（32）把背压传导给读循环，避免 shell 高频输出撑爆内存。
    // tx 本体保留在主路径用于主动 drop 关闭通道；reader 任务用 clone。
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    let tx_reader = tx.clone();
    let reader_task = tokio::task::spawn_blocking(move || pty_read_loop(master_reader, &tx_reader));

    // TCP → PTY：tokio 读 half → std writer + resize 轮询。
    // pty_master move 进 async block：take_writer 后仍可调用 resize()（&self）。
    // 每次 TCP 数据到达写入 PTY 后，non-blocking poll resize_rx 处理积压的
    // resize 信号。PTY master 写是 blocking fd 但写入缓冲很小，不会长期阻塞。
    let tcp_to_pty = tokio::spawn(async move {
        let mut tcp_killer = tcp_killer;
        let mut resize_rx = resize_rx;
        let mut master_writer = pty_master.take_writer().map_err(std::io::Error::other)?;
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
                    // non-blocking poll resize 事件：高频 resize 场景下数据写入后
                    // 立即检查，保证 resize 延迟 ≤ 单次 TCP read 周期。
                    // pty_master 仍在此 async block 的作用域内（take_writer 不消费
                    // master），可直接调用 resize()。
                    while let Ok((rows, cols)) = resize_rx.try_recv() {
                        let size = PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        };
                        if let Err(e) = pty_master.resize(size) {
                            tracing::warn!("PTY resize failed: {e}");
                        }
                    }
                }
            }
        }
        Ok::<(), std::io::Error>(())
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

    // 注销 PTY_REGISTRY（有 id 时）。
    if let Some(id) = &registry_id {
        PTY_REGISTRY.lock().await.remove(id);
    }
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

    #[test]
    fn negotiation_parses_with_id() {
        let n: Negotiation =
            serde_json::from_str(r#"{"rows":24,"cols":80,"id":"abc-123"}"#).unwrap();
        assert_eq!(n.rows, 24);
        assert_eq!(n.cols, 80);
        assert_eq!(n.id.as_deref(), Some("abc-123"));
        assert!(n.resize_for.is_none());
    }

    #[test]
    fn negotiation_parses_resize_for() {
        let n: Negotiation =
            serde_json::from_str(r#"{"resize_for":"abc-123","rows":50,"cols":120}"#).unwrap();
        assert_eq!(n.resize_for.as_deref(), Some("abc-123"));
        assert_eq!(n.rows, 50);
        assert_eq!(n.cols, 120);
        assert!(n.id.is_none());
    }

    #[test]
    fn negotiation_unknown_fields_ignored() {
        // Negotiation 无 deny_unknown_fields：旧客户端不发 id/resize_for 时
        // serde 默认忽略多余字段；新字段存在时正常解析。
        let n: Negotiation = serde_json::from_str(
            r#"{"rows":24,"cols":80,"shell":"bash","unknown":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(n.rows, 24);
        assert!(n.id.is_none());
        assert!(n.resize_for.is_none());
    }

    /// 验证 resize_for 协商帧到达时：查找 PTY_REGISTRY 发送 resize 信号后关闭，
    // 不建立新 shell。由于 PTY_REGISTRY 是全局 static，此测试验证 resize_for
    // 解析和 registry 查找逻辑——真实 resize 通道传递需端到端测试覆盖。
    #[tokio::test]
    async fn resize_for_sends_signal_and_closes() {
        let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(8);
        let test_id = "test-resize-id".to_string();
        PTY_REGISTRY.lock().await.insert(test_id.clone(), resize_tx);

        // 构造 resize_for 协商帧的 TCP 连接：写入 JSON + '\n'，然后关闭写端。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = handle_connection(stream).await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::AsyncWriteExt;
        client
            .write_all(
                format!(
                    r#"{{"resize_for":"{}","rows":50,"cols":120}}"#,
                    test_id
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client.write_all(b"\n").await.unwrap();
        drop(client); // 关闭让服务端正常返回

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;

        // 验证 resize 信号已发送
        let signal = tokio::time::timeout(std::time::Duration::from_millis(100), resize_rx.recv())
            .await
            .expect("resize signal should arrive")
            .expect("channel should not be closed");
        assert_eq!(signal, (50, 120));

        // resize_for 路径不触碰 registry（只查找发送），测试自行清理注册的条目。
        PTY_REGISTRY.lock().await.remove(&test_id);
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
