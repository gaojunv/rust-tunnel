//! 会话内的文件传输编排（控制器与被控端共用一套状态机）。
//!
//! # 方向
//!
//! 只有**控制器**能发起操作（浏览/下载/上传），被控端只应答。这是有意的：被控端是「被
//! 接管」的一方，让它反过来浏览控制者的磁盘会扩大攻击面，也没有实际需求。
//!
//! | 操作 | 发起方发出的请求 | 对端应答 | 谁发 `FileChunk` |
//! |---|---|---|---|
//! | 浏览 | `FileListRequest` | `FileList` | — |
//! | 下载 | `FileDownloadRequest` | `FileAccept` | **被控端** |
//! | 上传 | `FileUploadRequest` | `FileAccept` | **控制器** |
//!
//! # 与画面共享一条 TCP 流
//!
//! 中继只给一条字节流，画面与文件天然共享带宽，所以「优先画面」只能靠**主动让出**实现：
//! 每发一块 [`CHUNK_PAUSE`] 让出调度，会话循环的 `select!` 就有机会把到期的画面帧插进来。
//! 块大小 [`CHUNK_SIZE`] 决定了单次占用的时长（64 KiB 在慢链路上也不会长时间独占写端）。
//!
//! 真正意义上的「车道优先级」（`Lane` 分通道 + 独立写任务）需要把写端拆成任务 + 优先队列，
//! 本轮不做；当前的让出策略在实测中足够，且不引入新的复杂度。
//!
//! # 同一时刻只允许一次传输
//!
//! 单流上并发多路传输只会互相拖慢，且 UI 也没法同时展示。第二次发起会被直接拒绝并报事件。

use crate::error::{RemoteError, RemoteResult};
use crate::file::{FileService, MAX_TRANSFER_BYTES};
use crate::screen::FileEntry;
use crate::session::SessionMessage;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// 单块字节数。
pub const CHUNK_SIZE: usize = 64 * 1024;

/// 两块之间的最小间隔：让出调度，把带宽让给画面帧（上限 ≈16 MiB/s）。
pub const CHUNK_PAUSE: std::time::Duration = std::time::Duration::from_millis(4);

/// 传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// 被控端 → 控制器。
    Download,
    /// 控制器 → 被控端。
    Upload,
}

impl TransferDirection {
    /// UI 中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Download => "下载",
            Self::Upload => "上传",
        }
    }
}

/// 一次传输的进度快照（UI 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    /// 传输 ID。
    pub id: u64,
    /// 文件名。
    pub name: String,
    /// 方向。
    pub direction: TransferDirection,
    /// 已传字节。
    pub done: u64,
    /// 总字节。
    pub total: u64,
}

/// 传输侧事件（推给 UI 状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// 目录列表回来了。
    Listed {
        /// 虚拟路径。
        path: String,
        /// 目录项。
        entries: Vec<FileEntry>,
    },
    /// 传输开始（已受理）。
    Started(TransferProgress),
    /// 进度推进。
    Progress {
        /// 传输 ID。
        id: u64,
        /// 已传字节。
        done: u64,
        /// 总字节（0 表示未知）。
        total: u64,
    },
    /// 传输完成。
    Finished {
        /// 传输 ID。
        id: u64,
        /// 本地落地路径（下载）或远端虚拟路径（上传）。
        path: String,
    },
    /// 传输失败/取消（`id == 0` 表示还没分配到 ID 就被拒）。
    Failed {
        /// 传输 ID。
        id: u64,
        /// 原因。
        reason: String,
    },
}

/// 控制器侧发起的文件操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferCommand {
    /// 列出远端目录。
    List {
        /// 虚拟路径。
        path: String,
    },
    /// 从远端下载。
    Download {
        /// 远端虚拟路径。
        path: String,
    },
    /// 上传本地文件到远端。
    Upload {
        /// 本端绝对路径。
        local: PathBuf,
        /// 远端虚拟路径（含文件名）。
        remote: String,
    },
    /// 取消一次传输。
    Cancel {
        /// 传输 ID。
        id: u64,
    },
}

/// 本端正在**发出**的一次传输。
struct Outgoing {
    /// 传输 ID。
    id: u64,
    /// 文件名（UI 展示）。
    name: String,
    /// 源文件。
    file: tokio::fs::File,
    /// 已发字节。
    done: u64,
    /// 总字节（0 表示未知）。
    total: u64,
}

/// 本端正在**接收**的一次传输。
struct Incoming {
    /// 传输 ID。
    id: u64,
    /// 落地文件。
    file: tokio::fs::File,
    /// 已收字节。
    done: u64,
    /// 总字节（0 表示未知）。
    total: u64,
    /// 本地落地路径（完成事件里回给 UI）。
    local_path: PathBuf,
}

/// 本端已发出 `FileUploadRequest`、正在等 `FileAccept` 的一次上传。
struct PendingUpload {
    /// 请求 ID（`FileAccept` 必须回同一个才认）。
    id: u64,
    /// 本地源文件（受理之后才打开句柄）。
    local: PathBuf,
    /// 远端虚拟目标路径（含文件名）。
    remote: String,
}

/// 本端已发出 `FileDownloadRequest`、正在等 `FileAccept` 的一次下载。
struct PendingDownload {
    /// 请求 ID（`FileAccept` 必须回同一个才认）。
    id: u64,
    /// 远端文件名（落地时的默认名）。
    name: String,
}

/// 文件传输状态机。
pub struct TransferHub {
    /// 本端作为被控端时的共享目录服务；`None` 表示本端不提供文件服务。
    service: Option<Arc<FileService>>,
    /// 本端作为控制器时的落盘根（下载默认落到这里）。
    local_root: PathBuf,
    /// 事件出口。
    events: mpsc::UnboundedSender<TransferEvent>,
    /// 在途出站传输。
    outgoing: Option<Outgoing>,
    /// 在途入站传输。
    incoming: Option<Incoming>,
    /// 已发出 `FileUploadRequest`、正在等 `FileAccept` 的上传。
    ///
    /// 槽位里**带着请求 id**：`FileAccept` 只有与它匹配时才允许开本地文件，否则被当成
    /// 未经请求/陈旧/重复的受理拒掉（见 [`TransferHub::on_accept`]）。
    pending_upload: Option<PendingUpload>,
    /// 已发出 `FileDownloadRequest`、正在等 `FileAccept` 的下载（同样带着请求 id）。
    pending_download: Option<PendingDownload>,
    /// 下一个传输/请求 ID。
    next_id: u64,
}

impl std::fmt::Debug for TransferHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferHub")
            .field("service", &self.service.is_some())
            .field("outgoing", &self.outgoing.is_some())
            .field("incoming", &self.incoming.is_some())
            .finish_non_exhaustive()
    }
}

impl TransferHub {
    /// 构造。
    ///
    /// `service` 为 `None` 时本端拒绝一切文件请求（被控端未配置共享目录）。
    ///
    /// UI 命令不从这里进：它们与输入/切档位共用会话循环的 `ControllerCommand` 通道，顺序天然
    /// 与用户操作一致（见 [`crate::controller::ControllerCommand`]）。
    #[must_use]
    pub fn new(
        service: Option<Arc<FileService>>,
        local_root: PathBuf,
        events: mpsc::UnboundedSender<TransferEvent>,
    ) -> Self {
        Self {
            service,
            local_root,
            events,
            outgoing: None,
            incoming: None,
            pending_upload: None,
            pending_download: None,
            next_id: 1,
        }
    }

    /// 是否有在途的出站数据（会话循环据此决定是否启用发送分支）。
    #[must_use]
    pub fn has_outgoing(&self) -> bool {
        self.outgoing.is_some()
    }

    /// 是否完全空闲（无在途传输，可跳过发送分支）。
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.outgoing.is_none() && self.incoming.is_none()
    }

    /// 分配一个 ID。
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// 发一条事件（UI 已关闭时忽略）。
    fn emit(&self, event: TransferEvent) {
        if self.events.send(event).is_err() {
            debug!("传输事件无人接收（UI 已关闭）");
        }
    }

    /// 把一条 UI 命令翻译成线上请求（`None` 表示命令已在本地消化，无需发包）。
    ///
    /// # Errors
    /// 上传时本地文件不存在或过大返回错误。
    pub async fn on_command(&mut self, cmd: TransferCommand) -> RemoteResult<Option<SessionMessage>> {
        match cmd {
            TransferCommand::List { path } => {
                let id = self.alloc_id();
                Ok(Some(SessionMessage::FileListRequest { id, path }))
            }
            TransferCommand::Download { path } => {
                if !self.is_idle() || self.pending_download.is_some() || self.pending_upload.is_some() {
                    self.busy();
                    return Ok(None);
                }
                let name = path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("download")
                    .to_owned();
                // 请求与待决槽位共用同一个 id：**发起时就分配并记进槽位**，`FileAccept` 回来
                // 时才有东西可对（旧槽位只存文件名/路径、不存 id，于是无从判断受理是不是自己发的，
                // 任何受理都被当成这一次的）。
                let id = self.alloc_id();
                self.pending_download = Some(PendingDownload { id, name });
                Ok(Some(SessionMessage::FileDownloadRequest { id, path }))
            }
            TransferCommand::Upload { local, remote } => {
                if !self.is_idle() || self.pending_download.is_some() || self.pending_upload.is_some() {
                    self.busy();
                    return Ok(None);
                }
                let size = tokio::fs::metadata(&local).await?.len();
                if size > MAX_TRANSFER_BYTES {
                    return Err(RemoteError::Platform(format!("文件过大（{size} 字节）")));
                }
                // 记住本地源与请求 id，等 `FileAccept` 回来且 id 匹配了再开句柄（被拒就不必开）。
                let id = self.alloc_id();
                self.pending_upload = Some(PendingUpload {
                    id,
                    local,
                    remote: remote.clone(),
                });
                Ok(Some(SessionMessage::FileUploadRequest { id, path: remote, size }))
            }
            TransferCommand::Cancel { id } => {
                self.abort_transfer(id);
                Ok(Some(SessionMessage::FileError {
                    id,
                    reason: "用户取消".to_owned(),
                }))
            }
        }
    }

    /// 报「已有进行中的传输」。
    fn busy(&self) {
        self.emit(TransferEvent::Failed {
            id: 0,
            reason: "已有进行中的传输，请等它结束".to_owned(),
        });
    }

    /// 处理一条入站 `File*` 消息，返回需要回发的消息。
    ///
    /// 非 `File*` 消息忽略（返回空 `Vec`）。
    ///
    /// # Errors
    /// 本地文件读写失败时返回 [`RemoteError::Io`]；调用方应据此收尾会话。
    pub async fn on_message(&mut self, msg: SessionMessage) -> RemoteResult<Vec<SessionMessage>> {
        let mut out = Vec::new();
        match msg {
            SessionMessage::FileListRequest { id, path } => self.on_list_request(id, path, &mut out).await,
            SessionMessage::FileList { path, entries, .. } => {
                self.emit(TransferEvent::Listed { path, entries });
            }
            SessionMessage::FileDownloadRequest { id, path } => {
                self.on_download_request(id, &path, &mut out).await;
            }
            SessionMessage::FileUploadRequest { id, path, size } => {
                self.on_upload_request(id, &path, size, &mut out).await;
            }
            SessionMessage::FileAccept {
                id,
                accepted,
                reason,
                size,
            } => self.on_accept(id, accepted, reason, size, &mut out).await?,
            SessionMessage::FileChunk { id, data } => self.on_chunk(id, data, &mut out).await?,
            SessionMessage::FileDone { id } => self.on_done(id).await?,
            SessionMessage::FileError { id, reason } => {
                // `abort_transfer` 现在会连 id 匹配的待决槽位一起撤掉，所以这里不再无差别地
                // 清空两个待决槽位——否则一个无关 id 的错误会把正当等待中的请求也抹掉。
                self.abort_transfer(id);
                self.emit(TransferEvent::Failed { id, reason });
            }
            _ => {}
        }
        Ok(out)
    }

    /// 列目录请求：本端作为被控端时回 `FileList`（未开共享或读失败则回空表，不报错）。
    async fn on_list_request(&self, id: u64, path: String, out: &mut Vec<SessionMessage>) {
        let entries = match self.service.clone() {
            Some(service) => match service.list(&path).await {
                Ok(entries) => entries,
                Err(err) => {
                    warn!(error = %err, %path, "列目录失败");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        out.push(SessionMessage::FileList { id, path, entries });
    }

    /// 下载请求：本端作为被控端时打开源文件，受理后开始推块。
    async fn on_download_request(&mut self, id: u64, path: &str, out: &mut Vec<SessionMessage>) {
        let Some(service) = self.service.clone() else {
            out.push(reject(id, "被控端未开启文件共享"));
            return;
        };
        if self.outgoing.is_some() {
            out.push(reject(id, "被控端已有进行中的传输"));
            return;
        }
        match service.open_read(path).await {
            Ok((meta, file)) => {
                out.push(accept(id, meta.size));
                self.emit(TransferEvent::Started(TransferProgress {
                    id,
                    name: meta.name.clone(),
                    direction: TransferDirection::Download,
                    done: 0,
                    total: meta.size,
                }));
                self.outgoing = Some(Outgoing {
                    id,
                    name: meta.name,
                    file,
                    done: 0,
                    total: meta.size,
                });
            }
            Err(err) => out.push(reject(id, &err.to_string())),
        }
    }

    /// 上传请求：本端作为被控端时创建落地文件，受理后开始收块。
    async fn on_upload_request(&mut self, id: u64, path: &str, size: u64, out: &mut Vec<SessionMessage>) {
        let Some(service) = self.service.clone() else {
            out.push(reject(id, "被控端未开启文件共享"));
            return;
        };
        if self.incoming.is_some() {
            out.push(reject(id, "被控端已有进行中的传输"));
            return;
        }
        match service.open_write(path, size).await {
            Ok((meta, file)) => {
                out.push(accept(id, size));
                self.emit(TransferEvent::Started(TransferProgress {
                    id,
                    name: meta.name.clone(),
                    direction: TransferDirection::Upload,
                    done: 0,
                    total: size,
                }));
                self.incoming = Some(Incoming {
                    id,
                    file,
                    done: 0,
                    total: size,
                    // `Path::join` 遇到绝对路径会整体替换，故虚拟路径必须先去前导 `/`。
                    local_path: service.root().join(meta.path.trim_start_matches('/')),
                });
            }
            Err(err) => out.push(reject(id, &err.to_string())),
        }
    }

    /// `FileAccept`：受理则按方向开本地源（上传）或落地文件（下载），拒绝则报失败。
    ///
    /// **受理只有在 id 与某个待决槽位匹配时才生效**：无待决请求、陈旧 id 或重复受理一律
    /// **不开本地文件**，只回一条 `FileError` 告知对端「这次受理没有对应请求」。否则已授权的
    /// 对端只要主动发一个 `accepted:true` + 一串 `FileChunk`，就能在本机凭空创建文件并写到
    /// [`MAX_TRANSFER_BYTES`]，用户全程无从取消。
    async fn on_accept(
        &mut self,
        id: u64,
        accepted: bool,
        reason: Option<String>,
        size: u64,
        out: &mut Vec<SessionMessage>,
    ) -> RemoteResult<()> {
        if !accepted {
            // 拒绝：只撤掉 id 匹配的那一个待决槽位，另一个（若有）必须原样留着。
            if self.pending_upload.as_ref().is_some_and(|p| p.id == id) {
                self.pending_upload = None;
            }
            if self.pending_download.as_ref().is_some_and(|p| p.id == id) {
                self.pending_download = None;
            }
            self.emit(TransferEvent::Failed {
                id,
                reason: reason.unwrap_or_else(|| "对端拒绝".to_owned()),
            });
            return Ok(());
        }
        if let Some(pending) = self.pending_upload.take_if(|p| p.id == id) {
            // 我们在上传：开本地源，开始推块。
            let file = tokio::fs::File::open(&pending.local).await?;
            let name = pending
                .remote
                .rsplit('/')
                .next()
                .unwrap_or(pending.remote.as_str())
                .to_owned();
            self.emit(TransferEvent::Started(TransferProgress {
                id,
                name: name.clone(),
                direction: TransferDirection::Upload,
                done: 0,
                total: size,
            }));
            self.outgoing = Some(Outgoing {
                id,
                name,
                file,
                done: 0,
                total: size,
            });
        } else if let Some(pending) = self.pending_download.take_if(|p| p.id == id) {
            // 我们在下载：落地到本端目录，开始收块。
            let dest = unique_path(&self.local_root, &pending.name).await;
            let file = tokio::fs::File::create(&dest).await?;
            self.emit(TransferEvent::Started(TransferProgress {
                id,
                name: pending.name.clone(),
                direction: TransferDirection::Download,
                done: 0,
                total: size,
            }));
            self.incoming = Some(Incoming {
                id,
                file,
                done: 0,
                total: size,
                local_path: dest,
            });
        } else {
            // 没有任何待决槽位匹配这个 id：未经请求的受理、陈旧 id 或重复受理。
            out.push(SessionMessage::FileError {
                id,
                reason: "未经请求的受理或受理 id 不匹配".to_owned(),
            });
        }
        Ok(())
    }

    /// 数据块：校验归属与长度上限后落盘。
    async fn on_chunk(&mut self, id: u64, data: Vec<u8>, out: &mut Vec<SessionMessage>) -> RemoteResult<()> {
        let Some(incoming) = self.incoming.as_mut() else {
            out.push(SessionMessage::FileError {
                id,
                reason: "收到无对应传输的数据块".to_owned(),
            });
            return Ok(());
        };
        if incoming.id != id {
            out.push(SessionMessage::FileError {
                id,
                reason: "数据块 ID 不匹配".to_owned(),
            });
            return Ok(());
        }
        let Some(next) = incoming.done.checked_add(data.len() as u64) else {
            out.push(SessionMessage::FileError {
                id,
                reason: "传输长度溢出".to_owned(),
            });
            self.abort_transfer(id);
            return Ok(());
        };
        if next > MAX_TRANSFER_BYTES {
            out.push(SessionMessage::FileError {
                id,
                reason: "传输超过上限".to_owned(),
            });
            self.abort_transfer(id);
            return Ok(());
        }
        let total = incoming.total;
        incoming.file.write_all(&data).await?;
        incoming.done = next;
        self.emit(TransferEvent::Progress {
            id,
            done: next,
            total,
        });
        Ok(())
    }

    /// `FileDone`：只有「本端在收」时它才是完成信号；本端刚发完最后一块时它是对端回的确认。
    async fn on_done(&mut self, id: u64) -> RemoteResult<()> {
        if let Some(incoming) = self.incoming.take_if(|i| i.id == id) {
            incoming.file.sync_all().await?;
            self.emit(TransferEvent::Finished {
                id,
                path: incoming.local_path.display().to_string(),
            });
        } else {
            debug!(id, "收到无对应入站传输的完成确认");
        }
        Ok(())
    }

    /// 产出下一块出站数据（`None` 表示出站已结束或本端空闲）。
    ///
    /// # Errors
    /// 源文件读取失败时返回 [`RemoteError::Io`]。
    pub async fn next_outbound(&mut self) -> RemoteResult<Option<SessionMessage>> {
        let Some(outgoing) = self.outgoing.as_mut() else {
            return Ok(None);
        };
        let mut buf = vec![0_u8; CHUNK_SIZE];
        let read = outgoing.file.read(&mut buf).await?;
        if read == 0 {
            let (id, name) = (outgoing.id, outgoing.name.clone());
            self.outgoing = None;
            self.emit(TransferEvent::Finished { id, path: name });
            return Ok(Some(SessionMessage::FileDone { id }));
        }
        buf.truncate(read);
        outgoing.done = outgoing.done.saturating_add(read as u64);
        let (id, done, total) = (outgoing.id, outgoing.done, outgoing.total);
        self.emit(TransferEvent::Progress { id, done, total });
        Ok(Some(SessionMessage::FileChunk { id, data: buf }))
    }

    /// 中断一次传输（只清本地状态；调用方决定要不要发 `FileError`）。
    ///
    /// **必须连 id 匹配的待决请求一起撤销**：用户在 `FileAccept` 回来之前取消时，若不把槽位
    /// 清掉，晚到的 `FileAccept{accepted:true}` 会命中同一个 id，把已经取消的传输重新拉起来
    /// （用户取消被无视）；而若 accept 永远不回来，陈旧的槽位又会让后续所有传输一直报
    /// 「已有进行中的传输」。
    pub fn abort_transfer(&mut self, id: u64) {
        if self.outgoing.as_ref().is_some_and(|o| o.id == id) {
            self.outgoing = None;
        }
        if self.incoming.as_ref().is_some_and(|i| i.id == id) {
            self.incoming = None;
        }
        if self.pending_upload.as_ref().is_some_and(|p| p.id == id) {
            self.pending_upload = None;
        }
        if self.pending_download.as_ref().is_some_and(|p| p.id == id) {
            self.pending_download = None;
        }
    }

    /// 会话内所有在途传输作废；返回需要回发给对端的 `FileError`（每个被中断的传输一条），
    /// 调用方负责写出。**必须回发**：发送方读文件失败时若不通知对端，对端会一直挂在半途。
    pub fn abort_all(&mut self, reason: &str) -> Vec<SessionMessage> {
        let mut out = Vec::new();
        if let Some(outgoing) = self.outgoing.take() {
            self.emit(TransferEvent::Failed {
                id: outgoing.id,
                reason: reason.to_owned(),
            });
            out.push(SessionMessage::FileError {
                id: outgoing.id,
                reason: reason.to_owned(),
            });
        }
        if let Some(incoming) = self.incoming.take() {
            self.emit(TransferEvent::Failed {
                id: incoming.id,
                reason: reason.to_owned(),
            });
            out.push(SessionMessage::FileError {
                id: incoming.id,
                reason: reason.to_owned(),
            });
        }
        // 待决请求还没进入在途，对端没有数据面需要收尾，只清本地槽位（与旧行为一致）。
        self.pending_upload = None;
        self.pending_download = None;
        out
    }
}

/// 受理一条请求。
fn accept(id: u64, size: u64) -> SessionMessage {
    SessionMessage::FileAccept {
        id,
        accepted: true,
        reason: None,
        size,
    }
}

/// 拒绝一条请求。
fn reject(id: u64, reason: &str) -> SessionMessage {
    SessionMessage::FileAccept {
        id,
        accepted: false,
        reason: Some(reason.to_owned()),
        size: 0,
    }
}

/// 在 `dir` 下取一个不冲突的文件名（`name` 已存在时依次尝试 `name (1)`、`name (2)`…）。
async fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.to_owned(), String::new()),
    };
    for n in 1..=9999_u32 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return candidate;
        }
    }
    // 兜底：加一个几乎不可能冲突的后缀。
    dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 带自清理的临时根目录（不引入 `tempfile` 依赖）；名字带进程 id + 原子序号，并发测试互不踩。
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("rt-remote-xfer-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn service(&self) -> Arc<FileService> {
            Arc::new(FileService::new(self.0.clone()))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn hub(
        service: Option<Arc<FileService>>,
        local_root: PathBuf,
    ) -> (TransferHub, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TransferHub::new(service, local_root, tx), rx)
    }

    /// 排空事件通道。
    fn drain(rx: &mut mpsc::UnboundedReceiver<TransferEvent>) -> Vec<TransferEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// 从回发消息里挑出 `accepted:true` 的 `FileAccept`，返回它声明的字节数。
    fn expect_accept(replies: &[SessionMessage]) -> u64 {
        replies
            .iter()
            .find_map(|m| match m {
                SessionMessage::FileAccept {
                    accepted: true, size, ..
                } => Some(*size),
                _ => None,
            })
            .expect("回发消息里应有 accepted:true 的 FileAccept")
    }

    /// 回发消息里是否有指向 `id` 的 `FileError`。
    fn has_file_error(msgs: &[SessionMessage], id: u64) -> bool {
        msgs.iter()
            .any(|m| matches!(m, SessionMessage::FileError { id: mid, .. } if *mid == id))
    }

    /// 造一段可复现的伪随机字节（仅用于内容比对）。
    fn pseudo_bytes(len: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push(u8::try_from(x >> 24).unwrap_or(0));
        }
        out
    }

    /// 起一次下载请求，取出它分配的 id。
    async fn start_download(hub: &mut TransferHub, path: &str) -> u64 {
        let req = hub
            .on_command(TransferCommand::Download { path: path.to_owned() })
            .await
            .unwrap()
            .expect("下载应发出请求");
        match req {
            SessionMessage::FileDownloadRequest { id, .. } => id,
            other => panic!("应为 FileDownloadRequest，实得 {other:?}"),
        }
    }

    /// 用户取消必须连待决请求一起撤销：晚到的 `FileAccept{accepted:true}` 不得把已取消的传输拉起来。
    #[tokio::test]
    async fn cancel_during_pending_voids_late_accept() {
        let local = TempRoot::new("cancel-pending");
        let (mut ctrl, _rx) = hub(None, local.0.clone());
        let id = start_download(&mut ctrl, "/a.txt").await;
        assert!(ctrl.pending_download.is_some(), "发起后应有待决槽位");

        let cancel = ctrl
            .on_command(TransferCommand::Cancel { id })
            .await
            .unwrap()
            .expect("取消应回一条 FileError");
        assert!(matches!(cancel, SessionMessage::FileError { id: cid, .. } if cid == id));
        assert!(ctrl.pending_download.is_none(), "取消应清掉待决槽位");

        // 晚到的受理：id 还在，但待决槽位已撤销 → 不得开传输、不得落盘。
        let out = ctrl
            .on_message(SessionMessage::FileAccept {
                id,
                accepted: true,
                reason: None,
                size: 10,
            })
            .await
            .unwrap();
        assert!(has_file_error(&out, id), "对晚到的受理应回 FileError");
        assert!(ctrl.incoming.is_none(), "不得进入收块状态");
        assert!(ctrl.is_idle(), "取消后应完全空闲");
        assert!(!local.0.join("a.txt").exists(), "取消后不得创建目标文件");
    }

    /// 未被请求的受理（含重复受理）不得落盘，且要回 `FileError`。
    #[tokio::test]
    async fn unrequested_and_duplicate_accept_are_rejected() {
        let local = TempRoot::new("unrequested-accept");
        let (mut ctrl, _rx) = hub(None, local.0.clone());

        // 无任何待决请求时的受理。
        let out = ctrl
            .on_message(SessionMessage::FileAccept {
                id: 999,
                accepted: true,
                reason: None,
                size: 5,
            })
            .await
            .unwrap();
        assert!(has_file_error(&out, 999), "未经请求的受理应回 FileError");
        assert!(ctrl.incoming.is_none(), "不得进入收块状态");

        // 正常发起并受理一次。
        let id = start_download(&mut ctrl, "/x.bin").await;
        let out = ctrl
            .on_message(SessionMessage::FileAccept {
                id,
                accepted: true,
                reason: None,
                size: 5,
            })
            .await
            .unwrap();
        assert!(out.is_empty(), "正常受理无需回包");
        assert!(ctrl.incoming.is_some(), "受理后应进入收块状态");
        assert!(local.0.join("x.bin").exists(), "受理后应落地文件");

        // 同一 id 再来一次（重复受理）：回 FileError，且不产生第二个文件。
        let out = ctrl
            .on_message(SessionMessage::FileAccept {
                id,
                accepted: true,
                reason: None,
                size: 5,
            })
            .await
            .unwrap();
        assert!(has_file_error(&out, id), "重复受理应回 FileError");
        let files: Vec<_> = std::fs::read_dir(&local.0).unwrap().collect();
        assert_eq!(files.len(), 1, "重复受理不得创建第二个文件");
    }

    /// `accepted:false` 只清 id 匹配的那一个待决槽位。
    #[tokio::test]
    async fn rejection_only_clears_matching_pending() {
        let local = TempRoot::new("reject-match");
        let (mut ctrl, mut rx) = hub(None, local.0.clone());
        ctrl.pending_upload = Some(PendingUpload {
            id: 7,
            local: local.0.join("src"),
            remote: "/r".to_owned(),
        });
        ctrl.pending_download = Some(PendingDownload {
            id: 8,
            name: "n".to_owned(),
        });

        let _ = ctrl
            .on_message(SessionMessage::FileAccept {
                id: 7,
                accepted: false,
                reason: Some("拒绝".to_owned()),
                size: 0,
            })
            .await
            .unwrap();

        assert!(ctrl.pending_upload.is_none(), "id 匹配的待决槽位应被清掉");
        assert!(ctrl.pending_download.is_some(), "另一个待决槽位必须原样保留");
        assert!(drain(&mut rx)
            .iter()
            .any(|e| matches!(e, TransferEvent::Failed { id: 7, .. })));
    }

    /// 无对应 `incoming` 的数据块被拒。
    #[tokio::test]
    async fn chunk_without_incoming_is_rejected() {
        let local = TempRoot::new("chunk-orphan");
        let (mut ctrl, _rx) = hub(None, local.0.clone());
        let out = ctrl
            .on_message(SessionMessage::FileChunk { id: 1, data: vec![0; 4] })
            .await
            .unwrap();
        assert!(has_file_error(&out, 1));
    }

    /// id 不匹配的数据块被拒，且不影响在途的 `incoming`。
    #[tokio::test]
    async fn chunk_id_mismatch_is_rejected() {
        let local = TempRoot::new("chunk-mismatch");
        let (mut ctrl, _rx) = hub(None, local.0.clone());
        let path = local.0.join("in.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();
        ctrl.incoming = Some(Incoming {
            id: 5,
            file,
            done: 0,
            total: 100,
            local_path: path,
        });

        let out = ctrl
            .on_message(SessionMessage::FileChunk { id: 6, data: vec![0; 4] })
            .await
            .unwrap();
        assert!(has_file_error(&out, 6));
        assert!(ctrl.incoming.is_some(), "不匹配的块不得殃及在途传输");
    }

    /// 长度溢出与超过 `MAX_TRANSFER_BYTES` 都回 `FileError` 并中断该次传输。
    #[tokio::test]
    async fn chunk_overflow_and_over_cap_abort() {
        let local = TempRoot::new("chunk-limit");
        let (mut ctrl, _rx) = hub(None, local.0.clone());

        let overflow = local.0.join("overflow.bin");
        let file = tokio::fs::File::create(&overflow).await.unwrap();
        ctrl.incoming = Some(Incoming {
            id: 1,
            file,
            done: u64::MAX - 1,
            total: 0,
            local_path: overflow,
        });
        let out = ctrl
            .on_message(SessionMessage::FileChunk { id: 1, data: vec![0; 4] })
            .await
            .unwrap();
        assert!(has_file_error(&out, 1), "溢出应回 FileError");
        assert!(ctrl.incoming.is_none(), "溢出应中断该次传输");

        let capped = local.0.join("capped.bin");
        let file = tokio::fs::File::create(&capped).await.unwrap();
        ctrl.incoming = Some(Incoming {
            id: 2,
            file,
            done: MAX_TRANSFER_BYTES,
            total: 0,
            local_path: capped,
        });
        let out = ctrl
            .on_message(SessionMessage::FileChunk { id: 2, data: vec![0; 1] })
            .await
            .unwrap();
        assert!(has_file_error(&out, 2), "超上限应回 FileError");
        assert!(ctrl.incoming.is_none(), "超上限应中断该次传输");
    }

    /// `abort_all` 对 outgoing / incoming 各回一条 `FileError` 并清空状态，本地 `Failed` 照发。
    #[tokio::test]
    async fn abort_all_reports_and_clears() {
        let local = TempRoot::new("abort-all");
        let (mut ctrl, mut rx) = hub(None, local.0.clone());
        let src = tokio::fs::File::create(local.0.join("out.bin")).await.unwrap();
        ctrl.outgoing = Some(Outgoing {
            id: 11,
            name: "out.bin".to_owned(),
            file: src,
            done: 0,
            total: 0,
        });
        let dst = tokio::fs::File::create(local.0.join("in.bin")).await.unwrap();
        ctrl.incoming = Some(Incoming {
            id: 12,
            file: dst,
            done: 0,
            total: 0,
            local_path: local.0.join("in.bin"),
        });

        let msgs = ctrl.abort_all("坏了");
        assert_eq!(msgs.len(), 2, "每个被中断的传输一条 FileError");
        assert!(has_file_error(&msgs, 11));
        assert!(has_file_error(&msgs, 12));
        assert!(ctrl.outgoing.is_none() && ctrl.incoming.is_none(), "状态应清空");

        let events = drain(&mut rx);
        assert!(events
            .iter()
            .any(|e| matches!(e, TransferEvent::Failed { id: 11, .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, TransferEvent::Failed { id: 12, .. })));
    }

    /// 已有在途/待决传输时，新的上传/下载被拒（报事件、不发请求）。
    #[tokio::test]
    async fn busy_rejects_second_transfer() {
        let local = TempRoot::new("busy");
        let (mut ctrl, mut rx) = hub(None, local.0.clone());
        let _ = start_download(&mut ctrl, "/first.bin").await;

        let second = ctrl
            .on_command(TransferCommand::Download {
                path: "/second.bin".to_owned(),
            })
            .await
            .unwrap();
        assert!(second.is_none(), "第二个下载不得发出请求");
        let src = local.0.join("local.bin");
        std::fs::write(&src, b"x").unwrap();
        let up = ctrl
            .on_command(TransferCommand::Upload {
                local: src.clone(),
                remote: "/u.bin".to_owned(),
            })
            .await
            .unwrap();
        assert!(up.is_none(), "待决期间的上传不得发出请求");
        assert!(drain(&mut rx)
            .iter()
            .any(|e| matches!(e, TransferEvent::Failed { id: 0, .. })));
    }

    /// 上传源超过上限被拒（发起阶段就挡下）。
    #[tokio::test]
    async fn upload_over_cap_is_rejected() {
        let local = TempRoot::new("upload-cap");
        let (mut ctrl, _rx) = hub(None, local.0.clone());
        let big = local.0.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_TRANSFER_BYTES + 1).unwrap();
        drop(file);

        let err = ctrl
            .on_command(TransferCommand::Upload {
                local: big,
                remote: "/big.bin".to_owned(),
            })
            .await;
        assert!(matches!(err, Err(RemoteError::Platform(_))), "超上限应报错");
    }

    /// `unique_path`：同名时依次 `name (1)`、`name (2)`，无扩展名也成立。
    #[tokio::test]
    async fn unique_path_avoids_conflicts() {
        let local = TempRoot::new("unique-path");
        std::fs::write(local.0.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&local.0, "a.txt").await, local.0.join("a (1).txt"));
        std::fs::write(local.0.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&local.0, "a.txt").await, local.0.join("a (2).txt"));
        assert_eq!(unique_path(&local.0, "b.txt").await, local.0.join("b.txt"));
        std::fs::write(local.0.join("noext"), b"x").unwrap();
        assert_eq!(unique_path(&local.0, "noext").await, local.0.join("noext (1)"));
    }

    /// 下载 happy path：请求 → 受理 → 推块 → 完成，两端状态机经消息来回即可跑通。
    #[tokio::test]
    async fn download_happy_path() {
        let share = TempRoot::new("dl-share");
        let dest = TempRoot::new("dl-dest");
        let payload = pseudo_bytes(150 * 1024);
        std::fs::write(share.0.join("source.bin"), &payload).unwrap();

        let (mut ctld, _ctld_rx) = hub(Some(share.service()), share.0.clone());
        let (mut ctrl, _ctrl_rx) = hub(None, dest.0.clone());

        let id = start_download(&mut ctrl, "/source.bin").await;
        let req = SessionMessage::FileDownloadRequest {
            id,
            path: "/source.bin".to_owned(),
        };

        let replies = ctld.on_message(req).await.unwrap();
        let size = expect_accept(&replies);
        assert_eq!(size, payload.len() as u64, "被控端应回报真实大小");
        ctrl.on_message(accept(id, size)).await.unwrap();
        assert!(ctrl.incoming.is_some(), "受理后控制器应进入收块状态");

        loop {
            let msg = ctld.next_outbound().await.unwrap().expect("应有出站消息");
            let done = matches!(msg, SessionMessage::FileDone { .. });
            ctrl.on_message(msg).await.unwrap();
            if done {
                break;
            }
        }

        assert_eq!(std::fs::read(dest.0.join("source.bin")).unwrap(), payload);
        assert!(ctrl.incoming.is_none(), "完成后应清空入站状态");
    }

    /// 上传 happy path：请求 → 受理 → 推块 → 完成，内容与源一致。
    #[tokio::test]
    async fn upload_happy_path() {
        let dest = TempRoot::new("ul-dest");
        let share = TempRoot::new("ul-share");
        let payload = pseudo_bytes(150 * 1024);
        let local = dest.0.join("upload-src.bin");
        std::fs::write(&local, &payload).unwrap();

        let (mut ctld, _ctld_rx) = hub(Some(share.service()), share.0.clone());
        let (mut ctrl, _ctrl_rx) = hub(None, dest.0.clone());

        let req = ctrl
            .on_command(TransferCommand::Upload {
                local,
                remote: "/up.bin".to_owned(),
            })
            .await
            .unwrap()
            .expect("上传应发出请求");
        let id = match req {
            SessionMessage::FileUploadRequest { id, .. } => id,
            other => panic!("应为 FileUploadRequest，实得 {other:?}"),
        };

        let replies = ctld.on_message(req).await.unwrap();
        let size = expect_accept(&replies);
        assert_eq!(size, payload.len() as u64);
        ctrl.on_message(accept(id, size)).await.unwrap();
        assert!(ctrl.outgoing.is_some(), "受理后控制器应开始推块");

        loop {
            let msg = ctrl.next_outbound().await.unwrap().expect("应有出站消息");
            let done = matches!(msg, SessionMessage::FileDone { .. });
            ctld.on_message(msg).await.unwrap();
            if done {
                break;
            }
        }

        assert_eq!(std::fs::read(share.0.join("up.bin")).unwrap(), payload);
        assert!(ctrl.outgoing.is_none(), "完成后应清空出站状态");
    }
}
