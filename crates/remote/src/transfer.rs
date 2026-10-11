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
    /// 已发出 `FileUploadRequest`、正在等 `FileAccept` 的本地源（路径, 远端路径）。
    pending_upload: Option<(PathBuf, String)>,
    /// 已发出 `FileDownloadRequest`、正在等 `FileAccept` 的远端文件名。
    pending_download: Option<String>,
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
                self.pending_download = Some(name);
                let id = self.alloc_id();
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
                // 记住本地源，等 `FileAccept` 到了再开句柄（被拒就不必开）。
                self.pending_upload = Some((local, remote.clone()));
                let id = self.alloc_id();
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
            } => self.on_accept(id, accepted, reason, size).await?,
            SessionMessage::FileChunk { id, data } => self.on_chunk(id, data, &mut out).await?,
            SessionMessage::FileDone { id } => self.on_done(id).await?,
            SessionMessage::FileError { id, reason } => {
                self.abort_transfer(id);
                self.pending_upload = None;
                self.pending_download = None;
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
    async fn on_accept(
        &mut self,
        id: u64,
        accepted: bool,
        reason: Option<String>,
        size: u64,
    ) -> RemoteResult<()> {
        if !accepted {
            self.pending_upload = None;
            self.pending_download = None;
            self.emit(TransferEvent::Failed {
                id,
                reason: reason.unwrap_or_else(|| "对端拒绝".to_owned()),
            });
            return Ok(());
        }
        if let Some((local, remote)) = self.pending_upload.take() {
            // 我们在上传：开本地源，开始推块。
            let file = tokio::fs::File::open(&local).await?;
            let name = remote.rsplit('/').next().unwrap_or(remote.as_str()).to_owned();
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
        } else {
            // 我们在下载：落地到本端目录，开始收块。
            let name = self.pending_download.take().unwrap_or_else(|| format!("file-{id}"));
            let dest = unique_path(&self.local_root, &name).await;
            let file = tokio::fs::File::create(&dest).await?;
            self.emit(TransferEvent::Started(TransferProgress {
                id,
                name: name.clone(),
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
    pub fn abort_transfer(&mut self, id: u64) {
        if self.outgoing.as_ref().is_some_and(|o| o.id == id) {
            self.outgoing = None;
        }
        if self.incoming.as_ref().is_some_and(|i| i.id == id) {
            self.incoming = None;
        }
    }

    /// 会话结束时清空所有在途传输。
    pub fn abort_all(&mut self, reason: &str) {
        if let Some(outgoing) = self.outgoing.take() {
            self.emit(TransferEvent::Failed {
                id: outgoing.id,
                reason: reason.to_owned(),
            });
        }
        if let Some(incoming) = self.incoming.take() {
            self.emit(TransferEvent::Failed {
                id: incoming.id,
                reason: reason.to_owned(),
            });
        }
        self.pending_upload = None;
        self.pending_download = None;
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
