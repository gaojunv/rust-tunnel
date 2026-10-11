//! 控制器侧会话循环：握手 → 收画面 → 回传输入 → 心跳。
//!
//! 传输建立阶段已把 `Hello` 帧与中继的 `RequestRelay` 帧合并写出（见 [`crate::transport`]），
//! 因此这里**只等 `HelloAck`**。收到后派生会话密钥、装上 [`crate::crypto::Cipher`]，
//! 之后的帧全部加密。

use crate::auth::verify_ack_mac;
use crate::crypto::{derive_session_keys, Cipher};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{decode_frame, FrameSink, InputEvent, QualityPreset};
use crate::session::{ControllerHandshake, SessionCodec, SessionFrame, SessionMessage, HEARTBEAT_INTERVAL};
use crate::tiles::decode_tiles;
use crate::transfer::{TransferCommand, TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 等待 `HelloAck` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 收尾 `Bye` 的发送上限。对端可能已停读（写缓冲满），不加限时会让 teardown 永久卡住、`on_closed`
/// 永不投递；`Bye` 只是尽力通知，超时丢弃即可。
const BYE_TIMEOUT: Duration = Duration::from_secs(2);

/// UI 投递给控制器会话的一条命令。
///
/// 会话循环只有一个输入口：把输入事件与「切显示器 / 切画质 / 剪贴板 / 文件」放在同一个通道里，
/// 顺序天然与用户操作一致（例如先切显示器再点鼠标，不会被重排到切换之前）。
#[derive(Debug)]
pub enum ControllerCommand {
    /// 一条输入事件。
    Input(InputEvent),
    /// 切换观看的显示器。
    SelectDisplay(u32),
    /// 切换画质档位。
    SetQuality(QualityPreset),
    /// 把本机剪贴板文本发给被控端。
    Clipboard(String),
    /// 请求被控端把它的剪贴板发过来。
    ClipboardRequest,
    /// 文件传输操作。
    File(TransferCommand),
}

/// 控制器会话配置。
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// 会话 ID。
    pub session_id: String,
    /// 服务端下发的一次性会话 token。**同时用于 KDF 认证**（见 [`crate::crypto`] 模块文档），
    /// 必须与被控端持有的那个一致，否则会话密钥不一致、第一帧即认证失败。
    pub token: String,
    /// 本端（控制器）RustDesk ID。
    pub local_rd_id: String,
    /// 会话模式。
    pub mode: RemoteMode,
}

/// 跑一次控制器会话，直到取消、对端关闭或出错。
///
/// `cmd_rx` 是 UI 经 [`crate::manager::RemoteManager`] 投递的命令流；`sink` 接收已解码画面
/// （[`FrameSink::on_frame`] / [`FrameSink::on_tiles`] 拿到的是 RGBA，不在 UI 线程解码）。
///
/// # Errors
/// 握手被拒、编解码失败、IO 失败时返回 [`RemoteError`]；正常结束（收到 `Bye` / 对端关闭 /
/// 取消）返回 `Ok(())`。
#[expect(
    clippy::too_many_lines,
    reason = "握手校验 + 画面/输入/剪贴板/文件/心跳的单会话循环，拆开反而丢失控制流全貌"
)]
pub async fn run_controller(
    duplex: BoxDuplex,
    config: ControllerConfig,
    handshake: ControllerHandshake,
    mut cmd_rx: mpsc::Receiver<ControllerCommand>,
    mut transfer: TransferHub,
    sink: std::sync::Arc<dyn FrameSink>,
    cancel: CancellationToken,
) -> RemoteResult<()> {
    let (read_half, write_half) = tokio::io::split(duplex);
    let mut reader = FramedRead::new(read_half, SessionCodec::new());
    let mut writer = FramedWrite::new(write_half, SessionCodec::new());

    // 1) 等 HelloAck（Hello 已由建立阶段流水线写出）。
    let ack = match timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
        Ok(Some(Ok(frame))) => frame,
        Ok(Some(Err(err))) => return Err(RemoteError::Codec(err.to_string())),
        Ok(None) => return Err(RemoteError::Handshake("对端在握手前关闭连接".to_owned())),
        Err(_) => return Err(RemoteError::Handshake("等待 HelloAck 超时".to_owned())),
    };
    let SessionMessage::HelloAck {
        accepted,
        reason,
        displays,
        x25519_pub,
        hmac_ack,
    } = ack.message
    else {
        return Err(RemoteError::Handshake(format!("期望 HelloAck，实得 {:?}", ack.message)));
    };
    if !accepted {
        return Err(RemoteError::Handshake(
            reason.unwrap_or_else(|| "被控端拒绝".to_owned()),
        ));
    }

    // 校验 HelloAck 的 MAC：证明被控端持有 token，且其临时公钥未被中继方替换。
    // 只构造不校验等于没做——这里必须真的比对。
    if !verify_ack_mac(
        &config.token,
        &config.session_id,
        &x25519_pub,
        &handshake.keypair.public_bytes(),
        &hmac_ack,
    ) {
        return Err(RemoteError::Handshake(
            "HelloAck 认证失败（token 不符或被中继方篡改）".to_owned(),
        ));
    }

    // 2) 派生密钥并装上加密（握手帧明文，此后全密文）。
    let shared = handshake.keypair.diffie_hellman(&x25519_pub)?;
    let responder_pub: [u8; 32] = x25519_pub
        .as_slice()
        .try_into()
        .map_err(|_| RemoteError::Auth("HelloAck 的 X25519 公钥长度非法".to_owned()))?;
    let keys = derive_session_keys(
        &config.token,
        &shared,
        &config.session_id,
        &handshake.keypair.public_bytes(),
        &responder_pub,
        true,
    );
    reader.decoder_mut().set_cipher(Cipher::new(keys.recv)?);
    writer.encoder_mut().set_cipher(Cipher::new(keys.send)?);
    debug!(session = %config.session_id, "控制器会话已加密");

    sink.on_displays(displays);

    // 3) 主循环。
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在有在途传输时启用（`has_outgoing` 为假时该分支不被轮询，不产生空转）。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reason = String::from("会话结束");
    let result = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(());
            }
            _ = heartbeat.tick() => {
                if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Heartbeat)).await {
                    break Err(err);
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                // 发送端被 drop = 会话已被上层收走，按正常结束处理。**不能 `continue`**：已关闭的
                // `mpsc::Receiver::recv()` 每次都立刻返回 `Ready(None)`，继续轮询会 100% 空转。
                let Some(cmd) = maybe_cmd else {
                    "会话命令通道已关闭".clone_into(&mut reason);
                    break Ok(());
                };
                match cmd {
                    ControllerCommand::Input(event) => {
                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Input(event))).await {
                            break Err(err);
                        }
                    }
                    ControllerCommand::SelectDisplay(display) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::SelectDisplay { display })).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::SetQuality(preset) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::SetQuality { preset })).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::Clipboard(text) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Clipboard { text })).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::ClipboardRequest => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::ClipboardRequest)).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::File(cmd) => match transfer.on_command(cmd).await {
                        Ok(Some(msg)) => {
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(msg)).await {
                                break Err(err);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => warn!(error = %err, "文件操作发起失败"),
                    },
                }
            }
            _ = file_tick.tick(), if transfer.has_outgoing() => {
                match transfer.next_outbound().await {
                    Ok(Some(msg)) => {
                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(msg)).await {
                            break Err(err);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "文件读取失败，中断该次传输");
                        // 回发 `FileError`：发送方不再有 chunk 流动，对端的 `Incoming` 会一直挂着。
                        if let Err(send_err) = send_file_errors(&mut writer, &cancel, transfer.abort_all(&err.to_string())).await {
                            break Err(send_err);
                        }
                    }
                }
            }
            incoming = reader.next() => {
                match incoming {
                    Some(Ok(frame)) => match frame.message {
                        SessionMessage::Video { display, seq, jpeg, .. } => {
                            match decode_frame(display, seq, &jpeg) {
                                Ok(decoded) => sink.on_frame(decoded),
                                Err(err) => warn!(error = %err, "画面解码失败，跳过该帧"),
                            }
                        }
                        SessionMessage::VideoTiles { display, width, height, seq, tiles } => {
                            match decode_tiles(display, seq, width, height, &tiles) {
                                Ok(update) => sink.on_tiles(update),
                                Err(err) => warn!(error = %err, "瓦片解码失败，跳过该批"),
                            }
                        }
                        SessionMessage::DisplayList(list) => sink.on_displays(list),
                        SessionMessage::Clipboard { text } => sink.on_clipboard(text),
                        SessionMessage::ClipboardRequest => {
                            // 引擎里**没有**任何路径会让被控端发出该请求（它只负责应答），且控制器
                            // 侧根本没有 `Clipboard` 实现可应答。收到即说明对端实现异常，记一条日志。
                            debug!("控制器收到 ClipboardRequest，但控制器不实现剪贴板应答，忽略");
                        }
                        SessionMessage::Heartbeat => {
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::HeartbeatAck)).await {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck => {}
                        SessionMessage::Bye { reason: r } => {
                            reason = r;
                            break Ok(());
                        }
                        file @ (SessionMessage::FileList { .. }
                        | SessionMessage::FileListRequest { .. }
                        | SessionMessage::FileDownloadRequest { .. }
                        | SessionMessage::FileUploadRequest { .. }
                        | SessionMessage::FileAccept { .. }
                        | SessionMessage::FileChunk { .. }
                        | SessionMessage::FileDone { .. }
                        | SessionMessage::FileError { .. }) => {
                            match transfer.on_message(file).await {
                                Ok(replies) => {
                                    let mut write_err = None;
                                    for reply in replies {
                                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(reply)).await {
                                            write_err = Some(err);
                                            break;
                                        }
                                    }
                                    if let Some(err) = write_err {
                                        break Err(err);
                                    }
                                }
                                Err(err) => {
                                    // 本地落盘失败：这次传输已不可信，报错并中断它，会话本身继续。
                                    warn!(error = %err, "文件接收失败，中断该次传输");
                                    if let Err(send_err) = send_file_errors(&mut writer, &cancel, transfer.abort_all(&err.to_string())).await {
                                        break Err(send_err);
                                    }
                                }
                            }
                        }
                        other => debug!(?other, "控制器收到未处理的消息"),
                    },
                    Some(Err(err)) => break Err(RemoteError::Codec(err.to_string())),
                    None => break Ok(()),
                }
            }
        }
    };

    // 取消也可能发生在某个 `send_cancellable` 里（对端停读、写缓冲填满时）：统一按「用户中断」
    // 正常收尾，与上面的 `cancel` 分支语义一致。
    let result = match result {
        Err(RemoteError::Cancelled) => {
            "用户中断".clone_into(&mut reason);
            Ok(())
        }
        other => other,
    };

    // 4) 收尾：尽力通知对端（限时，见 `BYE_TIMEOUT`），并把在途传输标记为失败（UI 不能停在 99%）。
    //    这里**不必**回发 `abort_all` 的 `FileError`——`Bye` 马上就到，对端收尾时同样会清掉自己
    //    的在途传输。
    let _ = transfer.abort_all(&reason);
    let _ = timeout(
        BYE_TIMEOUT,
        writer.send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() })),
    )
    .await;
    sink.on_closed(reason);
    result
}

/// 可取消的发送：`select!` 的 handler 里直接 `.await` 会让循环看不见取消信号。
///
/// tokio 的 `select!` 在 handler 求值期间**不再轮询其它分支**，所以对端停读（中继停走、TCP
/// 零窗口）把写缓冲填满后，`writer.send` 会永久挂起，`cancel.cancelled()` 分支再也得不到轮询
/// ——会话不结束、不回 `RemoteSessionEnd`、socket 与任务一起泄漏到 TCP 超时。故每次发送都同时
/// 等取消令牌，取消优先（`biased`）。
///
/// 发送被中途取消是安全的：会话正要结束，半截帧不会被对端当成完整帧（长度前缀 + bincode
/// 自校验），且此后不会再发新帧。
///
/// # Errors
/// 取消令牌触发时返回 [`RemoteError::Cancelled`]；发送失败时透传底层 IO 错误。
async fn send_cancellable(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    frame: SessionFrame,
) -> RemoteResult<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(RemoteError::Cancelled),
        res = writer.send(frame) => res.map_err(RemoteError::Io),
    }
}

/// 把 [`TransferHub::abort_all`] 返回的 `FileError` 依次写出。
///
/// **必须回发**：发送方读文件失败后不会再有任何 chunk 流动，对端既等不到 `FileDone` 也等不到
/// 错误，会一直挂在半途（UI 停在 99%）。写失败按会话级错误处理，与其它发送一致。
async fn send_file_errors(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    msgs: Vec<SessionMessage>,
) -> RemoteResult<()> {
    for msg in msgs {
        send_cancellable(writer, cancel, SessionFrame::file(msg)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ack_mac;
    use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
    use crate::error::RemoteResult;
    use crate::screen::{DecodedFrame, DisplayInfo, FrameSink, TileUpdate};
    use crate::session::{plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage};
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
    use rust_tunnel_common::protocol::RemoteMode;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{ReadHalf, WriteHalf};
    use tokio::sync::mpsc;
    use tokio_util::codec::{FramedRead, FramedWrite};
    use tokio_util::sync::CancellationToken;

    /// 单条等待上限，避免测试挂死。
    const WAIT: Duration = Duration::from_secs(5);

    type PeerRead = FramedRead<ReadHalf<BoxDuplex>, SessionCodec>;
    type PeerWrite = FramedWrite<WriteHalf<BoxDuplex>, SessionCodec>;

    /// 记录 `on_closed` 的假上报目标。
    #[derive(Default)]
    struct FakeSink {
        closed: Mutex<Option<String>>,
    }

    impl FrameSink for FakeSink {
        fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
        fn on_frame(&self, _frame: DecodedFrame) {}
        fn on_tiles(&self, _update: TileUpdate) {}
        fn on_closed(&self, reason: String) {
            if let Ok(mut guard) = self.closed.lock() {
                *guard = Some(reason);
            }
        }
    }

    /// 空闲的传输状态机（这些用例不测文件传输）。
    fn transfer_hub() -> (TransferHub, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TransferHub::new(None, std::env::temp_dir(), tx), rx)
    }

    /// 作为「被控端」完成握手：回一个合法的 `HelloAck`，返回装好加密的读写半边。
    ///
    /// 会话循环不发 `Hello`（由建立阶段流水线写出），故这里直接用 `plan` 里的控制器临时公钥
    /// 算 MAC，无需真的先读一帧。
    async fn controlled_peer_handshake(
        stream: BoxDuplex,
        plan: &ControllerHandshake,
        token: &str,
        session_id: &str,
    ) -> (PeerRead, PeerWrite) {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let mut writer = FramedWrite::new(write_half, SessionCodec::new());

        let responder = EphemeralKeyPair::generate();
        let responder_pub = responder.public_bytes();
        let initiator_pub = plan.keypair.public_bytes();
        let mac = ack_mac(token, session_id, &responder_pub, &initiator_pub);
        writer
            .send(SessionFrame::control(SessionMessage::HelloAck {
                accepted: true,
                reason: None,
                displays: Vec::new(),
                x25519_pub: responder_pub.to_vec(),
                hmac_ack: mac,
            }))
            .await
            .expect("回 HelloAck 应成功");

        let shared = responder.diffie_hellman(&initiator_pub).expect("ECDH 应成功");
        // 本端扮演被控端（响应方）：`initiator=false`。
        let keys = derive_session_keys(token, &shared, session_id, &initiator_pub, &responder_pub, false);
        reader
            .decoder_mut()
            .set_cipher(Cipher::new(keys.recv).expect("密钥长度合法"));
        writer
            .encoder_mut()
            .set_cipher(Cipher::new(keys.send).expect("密钥长度合法"));
        (reader, writer)
    }

    /// 后台把对端读端排空（用于让收尾的 `Bye` 能落地）。
    fn spawn_drain(mut reader: PeerRead) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(item) = reader.next().await {
                if item.is_err() {
                    break;
                }
            }
        })
    }

    fn config(session_id: &str, token: &str) -> ControllerConfig {
        ControllerConfig {
            session_id: session_id.to_owned(),
            token: token.to_owned(),
            local_rd_id: "111111111".to_owned(),
            mode: RemoteMode::Control,
        }
    }

    fn controller_task(
        stream: BoxDuplex,
        plan: ControllerHandshake,
        cmd_rx: mpsc::Receiver<ControllerCommand>,
        sink: Arc<dyn FrameSink>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<RemoteResult<()>> {
        tokio::spawn(run_controller(
            stream,
            config("sess-1", "TOK"),
            plan,
            cmd_rx,
            transfer_hub().0,
            sink,
            cancel,
        ))
    }

    /// 1. 取消必须能打断「已阻塞在发送里」的会话循环（核心回归）。
    ///
    /// 对端**完全不读**，先灌一条 1 MiB 的命令把写缓冲填满，使循环卡在 `send_cancellable`；
    /// 此时 fire 取消，循环必须脱身并让整个 `run_controller` 返回。修复前该发送发生在 `select!`
    /// 的 handler 内，取消分支得不到轮询，循环会一直阻塞到测试超时。
    ///
    /// 注意**不能**在取消后再去读对端：读端一消费，被卡住的发送就能推进，连修复前的代码也会
    /// 「自行恢复」，测试就测不到取消了。故这里对端自始至终不读，收尾的 `Bye` 只能等满
    /// `BYE_TIMEOUT`（2s），返回时间以 3s 为上界。
    #[tokio::test]
    async fn cancel_interrupts_blocked_send() {
        // duplex 容量远小于要发的帧：写缓冲很快填满，`send` 必然挂起。
        let (a, b) = tokio::io::duplex(16 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        // 对端**完全不读**：读半边攥着不放（drop 掉会让控制器写拿到 broken pipe，就测不到阻塞了）。
        let (_peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel.clone());

        // 等握手进入主循环，再灌一条大命令把写缓冲填满。
        tokio::time::sleep(Duration::from_millis(100)).await;
        cmd_tx
            .send(ControllerCommand::Clipboard("x".repeat(1_000_000)))
            .await
            .expect("投递命令");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 此刻循环卡在 `send_cancellable` 里。fire 取消 → 必须脱身。
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("取消后应脱身并返回（修复前会永久阻塞在 handler 内的发送里）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
    }

    /// 2. 取消后不能卡在收尾：即便对端**始终不读**，`Bye` 也会因限时而放弃，并在其后投递
    ///    `on_closed`（覆盖 `BYE_TIMEOUT`）。
    #[tokio::test]
    async fn cancel_does_not_block_teardown() {
        let (a, b) = tokio::io::duplex(16 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (_peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel.clone());

        tokio::time::sleep(Duration::from_millis(100)).await;
        cmd_tx
            .send(ControllerCommand::Clipboard("x".repeat(1_000_000)))
            .await
            .expect("投递命令");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 对端**始终不读**：收尾 `Bye` 发不出去，只能等 `BYE_TIMEOUT`；但 teardown 必须继续，
        // `on_closed` 必须照投——限时失效会在这里超时。
        cancel.cancel();
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("取消后整个 run_controller 应在有限时间内返回（Bye 限时失效会出现此超时）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
        assert_eq!(
            sink.closed.lock().unwrap().as_deref(),
            Some("用户中断"),
            "收尾原因应为用户中断，说明 on_closed 在 Bye 限时后仍被投递"
        );
    }

    /// 3. 命令通道关闭时循环必须退出，而不是空转（`recv()` 对已关闭通道每次都 `Ready(None)`）。
    #[tokio::test]
    async fn closed_command_channel_exits_loop() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;
        let _drain = spawn_drain(peer_reader);

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControllerCommand>(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel);

        // 等进入主循环后 drop 命令发送端。
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(cmd_tx);

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("命令通道关闭后循环应立即退出，而不是空转")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "命令通道关闭应按正常结束处理，实得 {result:?}");
        assert_eq!(
            sink.closed.lock().unwrap().as_deref(),
            Some("会话命令通道已关闭"),
            "收尾原因应指明命令通道关闭"
        );
    }
}
