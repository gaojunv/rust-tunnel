//! 客户端 GUI 自更新控制面：版本判定与文件分片推送。
//!
//! # 为什么复用控制通道
//!
//! GUI 客户端手里只有客户端 token，没有管理员 JWT。为自更新新开一个 HTTP 鉴权
//! 面，等于让「更新源」另立一套凭证体系，还要在浏览器之外再处理一次认证。控制
//! 通道本身已经加密、已经过 token 校验，客户端注册后即可双向收发消息，因此版本
//! 通告与文件传输都复用它：服务端不新增任何 HTTP 入口，客户端也不需要第二份凭证。
//!
//! # 分片与并发
//!
//! 控制通道单条消息上限 1 MiB（见 `rust_tunnel_common::protocol`），而 GUI 主程序
//! 通常有几 MB 到几十 MB，故按 [`CHUNK_SIZE`]（256 KiB）顺序分片：每条
//! [`ControlMessage::ClientUpdateChunk`] 带 `transfer_id` + `offset`，最后一片
//! `data` 为空且 `last = true`。同一客户端同一时刻只允许一个传输
//! （[`ClientEntry::update_transfer_active`]），重复请求会被明确拒绝，避免两条
//! 传输的偏移交叉写到客户端同一个临时文件里。
//!
//! # 双向版本门控
//!
//! 老客户端不认识 `ClientUpdate*` 变体，收到会因 bincode 解码失败直接断开控制
//! 通道；老服务端同理也不认识上行变体。因此两端都按
//! [`client_supports_update`] 门控：服务端只在客户端版本 ≥
//! [`crate::agent::runner::MIN_UPDATE_CLIENT_VERSION`] 时下发任何更新消息；客户端
//! 只在收到过服务端的更新消息后才上行 `ClientUpdateCheck` / `ClientUpdateRequest`。
//! 服务端即使「已是最新」也**必须**回一条 [`ControlMessage::ClientUpdateUpToDate`]，
//! 正是为了让客户端拿到「对端理解更新协议」的凭据——否则客户端永远不会主动检查。

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tracing::{debug, warn};

use crate::agent::runner::{client_supports_update, parse_version};
use crate::control_plane::client_registry::{ClientEntry, ControlSender};
use crate::control_plane::state::ServerState;
use crate::mgmt::api::client_downloads::{
    is_auto_update_file, is_safe_segment, resolve_download_path, scan_impl, semver_key,
    ClientDownloadFile, ClientDownloadVersion,
};
use rust_tunnel_common::{ClientUpdateFile, ControlMessage};

/// 单个分片的字节数。
///
/// 256 KiB 远低于控制通道 1 MiB 上限，既给 bincode 头留足余量，也不会让单条
/// 消息长时间独占总线（分片之间其它控制消息仍可插队）。
const CHUNK_SIZE: usize = 256 * 1024;

/// 一次更新判定的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateDecision {
    /// 归档里存在比客户端更新、且可直接替换本机程序（见
    /// [`is_auto_update_file`]）的 GUI 版本。
    Available {
        /// 最新版本目录名（如 `v0.15.0`）。
        version: String,
        /// 可自动替换的文件清单，全部带 SHA-256。
        files: Vec<ClientUpdateFile>,
    },
    /// 客户端已是最新，或归档里没有更新的可用 GUI 版本。
    UpToDate {
        /// 服务端已知的最新 GUI 版本；归档里没有 GUI 版本时回显客户端自己的版本。
        version: String,
    },
    /// 客户端版本缺失/非法/过旧，**不应下发任何**更新消息（下发会让它断连）。
    Unsupported,
}

/// 纯函数：按归档扫描结果判定客户端是否需要更新。
///
/// `versions` 必须与 [`scan_impl`] 的输出同序（语义化版本降序），故第一个命中的
/// 版本即最新。判定规则：
///
/// - 客户端版本缺失、非法或低于最低支持版本 → [`UpdateDecision::Unsupported`]；
/// - 存在严格高于客户端版本、且至少含一个可自动替换文件的版本 → `Available`，
///   只把可自动替换的文件下发（缺 SHA-256 的一律跳过）；
/// - 否则 `UpToDate`，版本号取归档里最新的可用 GUI 版本，没有则回显客户端版本。
#[must_use]
pub(crate) fn select_update(
    client_version: Option<&str>,
    versions: &[ClientDownloadVersion],
) -> UpdateDecision {
    if !client_supports_update(client_version) {
        return UpdateDecision::Unsupported;
    }
    let Some(client_key) = client_version.and_then(parse_version) else {
        return UpdateDecision::Unsupported;
    };

    // 归档已按语义化版本降序排列，第一个「含可自动替换文件」的版本即最新可用版本。
    let newest_with_files = versions
        .iter()
        .find(|version| version.files.iter().any(is_auto_update_file));

    let newer = versions.iter().find(|version| {
        semver_key(&version.version).is_some_and(|key| key > client_key)
            && version.files.iter().any(is_auto_update_file)
    });

    match newer {
        Some(version) => UpdateDecision::Available {
            version: version.version.clone(),
            files: version
                .files
                .iter()
                .filter(|file| is_auto_update_file(file))
                .filter_map(to_wire_file)
                .collect(),
        },
        None => UpdateDecision::UpToDate {
            version: newest_with_files.map_or_else(
                || client_version.unwrap_or_default().to_string(),
                |version| version.version.clone(),
            ),
        },
    }
}

/// 把归档里的文件描述转成下发形状；缺 SHA-256 的文件跳过（客户端无法校验就不许替换）。
fn to_wire_file(file: &ClientDownloadFile) -> Option<ClientUpdateFile> {
    Some(ClientUpdateFile {
        name: file.name.clone(),
        os: file.os.clone(),
        arch: file.arch.clone(),
        size: file.size,
        sha256: file.sha256.clone()?,
    })
}

/// 注册后/客户端主动检查时，下发最新 GUI 版本或「已是最新」。
///
/// 客户端不支持更新协议、服务端没配归档目录、或控制通道已关闭时静默返回——更新
/// 通告是**尽力而为**的附加信息，任何失败都不应影响注册与后续控制消息。
pub(crate) async fn push_update_status(state: &ServerState, entry: &Arc<ClientEntry>) {
    if !client_supports_update(entry.client_version.as_deref()) {
        return;
    }
    let Some(base) = state.client_dist_dir.as_deref() else {
        // 未配置归档目录：一条更新消息都不发。客户端因此不会上行任何 `ClientUpdate*`。
        return;
    };

    let scan = scan_impl(base).await;
    let message = match select_update(entry.client_version.as_deref(), &scan.versions) {
        UpdateDecision::Available { version, files } => {
            ControlMessage::ClientUpdateAvailable { version, files }
        }
        UpdateDecision::UpToDate { version } => ControlMessage::ClientUpdateUpToDate { version },
        // 门控已在函数开头判过；这里只是让类型穷尽，静默返回。
        UpdateDecision::Unsupported => return,
    };

    if entry.control_sender.send(message).await.is_err() {
        debug!(client = %entry.name, "控制通道已关闭，更新状态未下发");
    }
}

/// 处理 `ClientUpdateCheck`：与注册时推送走同一套逻辑。
pub(crate) async fn handle_update_check(state: &ServerState, entry: &Arc<ClientEntry>) {
    push_update_status(state, entry).await;
}

/// 处理 `ClientUpdateRequest`：校验路径与文件名后起一个任务分片推送。
///
/// 分片在独立任务里推送，读循环不会被一个几十 MB 的文件阻塞。所有失败都以
/// [`ControlMessage::ClientUpdateFailed`] 告知客户端（尽力而为），并无论如何都会
/// 释放 [`ClientEntry::update_transfer_active`]，避免一次失败把该客户端的更新通道
/// 永久锁死。
pub(crate) async fn handle_update_request(
    state: &ServerState,
    entry: &Arc<ClientEntry>,
    transfer_id: u32,
    version: &str,
    file: &str,
) {
    // 老客户端不可能合法发送本消息（上行同样双向门控）。真收到时也**不回包**：
    // 任何 `ClientUpdate*` 回包都会让不认识它的客户端反序列化失败并断连。
    if !client_supports_update(entry.client_version.as_deref()) {
        debug!(client = %entry.name, "不支持更新协议的客户端请求更新，已忽略");
        return;
    }

    // 同一客户端同一时刻只允许一个传输：两条传输会交叉写入客户端的临时文件。
    if entry
        .update_transfer_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        send_failure(
            entry,
            transfer_id,
            "已有更新传输在进行中，请等待其完成".to_string(),
        )
        .await;
        return;
    }

    let Some(base) = state.client_dist_dir.as_deref() else {
        entry.update_transfer_active.store(false, Ordering::SeqCst);
        send_failure(entry, transfer_id, "服务端未配置客户端归档目录".to_string()).await;
        return;
    };

    // 路径校验与 HTTP 下载端点同一把尺子：段校验 + 规范化后仍在归档目录内。
    let path =
        match resolve_download_path(base, version, file, "client archive directory unavailable") {
            Ok(path) => path,
            Err((_, message)) => {
                entry.update_transfer_active.store(false, Ordering::SeqCst);
                send_failure(entry, transfer_id, message.to_string()).await;
                return;
            }
        };

    // 即便路径合法，也只放行能就地替换的 GUI 主程序（`SHA256SUMS`、zip、dmg、
    // CLI 二进制与服务 exe 一律拒绝）。
    if !is_safe_segment(file) || !is_directly_replaceable(file) {
        entry.update_transfer_active.store(false, Ordering::SeqCst);
        send_failure(entry, transfer_id, "该文件不支持自动更新".to_string()).await;
        return;
    }

    let task_entry = Arc::clone(entry);
    let sender = entry.control_sender.clone();
    tokio::spawn(async move {
        if let Err(message) = stream_chunks(&sender, transfer_id, &path).await {
            warn!(
                client = %task_entry.name,
                transfer_id,
                "更新文件分片推送失败：{message}"
            );
            // 尽力告知原因；控制通道已关闭时这次发送同样会失败，属预期。
            let _ = sender
                .send(ControlMessage::ClientUpdateFailed {
                    transfer_id,
                    message,
                })
                .await;
        }
        task_entry
            .update_transfer_active
            .store(false, Ordering::SeqCst);
    });
}

/// 与 [`is_auto_update_file`] 的命名/扩展名部分同构：只放行 GUI 主程序（`.exe`
/// 或无扩展名的裸二进制）。
fn is_directly_replaceable(file: &str) -> bool {
    file.starts_with("rust-tunnel-client-gui-")
        && file
            .rsplit_once('.')
            .is_none_or(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
}

/// 尽力回传失败原因；控制通道已关闭时静默丢弃。
async fn send_failure(entry: &Arc<ClientEntry>, transfer_id: u32, message: String) {
    let _ = entry
        .control_sender
        .send(ControlMessage::ClientUpdateFailed {
            transfer_id,
            message,
        })
        .await;
}

/// 把文件按 [`CHUNK_SIZE`] 分片写入控制通道，最后追加一个空数据的结束片。
async fn stream_chunks(
    sender: &ControlSender,
    transfer_id: u32,
    path: &Path,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|err| format!("打开更新文件失败：{err}"))?;
    let mut buffer = vec![0_u8; CHUNK_SIZE];
    let mut offset = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|err| format!("读取更新文件失败：{err}"))?;
        if read == 0 {
            break;
        }
        sender
            .send(ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset,
                data: buffer[..read].to_vec(),
                last: false,
            })
            .await
            .map_err(|_| "控制通道已关闭".to_string())?;
        offset += read as u64;
    }
    sender
        .send(ControlMessage::ClientUpdateChunk {
            transfer_id,
            offset,
            data: Vec::new(),
            last: true,
        })
        .await
        .map_err(|_| "控制通道已关闭".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个可自动替换的 GUI 主程序文件描述。
    fn gui_exe(name: &str, sha256: Option<&str>) -> ClientDownloadFile {
        ClientDownloadFile {
            name: name.to_string(),
            os: "windows".into(),
            arch: "x86_64".into(),
            format: Some("exe".into()),
            size: 4096,
            sha256: sha256.map(str::to_string),
        }
    }

    /// 造一个 CLI 二进制文件描述（不可自动替换）。
    fn cli_binary(name: &str) -> ClientDownloadFile {
        ClientDownloadFile {
            name: name.to_string(),
            os: "linux".into(),
            arch: "x86_64".into(),
            format: None,
            size: 2048,
            sha256: Some("b".repeat(64)),
        }
    }

    /// 造一个版本目录描述。
    fn version(name: &str, files: Vec<ClientDownloadFile>) -> ClientDownloadVersion {
        ClientDownloadVersion {
            version: name.to_string(),
            is_latest: false,
            modified_at: None,
            files,
        }
    }

    /// 64 位十六进制摘要占位。
    fn sha() -> String {
        "a".repeat(64)
    }

    #[test]
    fn newer_gui_version_is_selected() {
        let digest = sha();
        let versions = vec![version(
            "v0.16.0",
            vec![gui_exe(
                "rust-tunnel-client-gui-windows-x86_64.exe",
                Some(&digest),
            )],
        )];
        let decision = select_update(Some("0.15.0"), &versions);
        assert_eq!(
            decision,
            UpdateDecision::Available {
                version: "v0.16.0".into(),
                files: vec![ClientUpdateFile {
                    name: "rust-tunnel-client-gui-windows-x86_64.exe".into(),
                    os: "windows".into(),
                    arch: "x86_64".into(),
                    size: 4096,
                    sha256: digest,
                }],
            }
        );
    }

    #[test]
    fn cli_only_newer_version_is_skipped() {
        // 最新版本只有 CLI 产物（GUI 还没发），不能当作更新推给 GUI 客户端。
        let digest = sha();
        let versions = vec![
            version(
                "v0.17.0",
                vec![cli_binary("rust-tunnel-client-linux-x86_64")],
            ),
            version(
                "v0.16.0",
                vec![gui_exe(
                    "rust-tunnel-client-gui-windows-x86_64.exe",
                    Some(&digest),
                )],
            ),
        ];
        let decision = select_update(Some("0.15.0"), &versions);
        match decision {
            UpdateDecision::Available { version, files } => {
                assert_eq!(version, "v0.16.0");
                assert_eq!(files.len(), 1);
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn same_version_is_up_to_date() {
        let digest = sha();
        let versions = vec![version(
            "v0.15.0",
            vec![gui_exe(
                "rust-tunnel-client-gui-windows-x86_64.exe",
                Some(&digest),
            )],
        )];
        assert_eq!(
            select_update(Some("0.15.0"), &versions),
            UpdateDecision::UpToDate {
                version: "v0.15.0".into()
            }
        );
        // 客户端带 `+agent` 构建元数据时同样按同一版本比较。
        assert_eq!(
            select_update(Some("0.15.0+agent"), &versions),
            UpdateDecision::UpToDate {
                version: "v0.15.0".into()
            }
        );
    }

    #[test]
    fn archive_older_than_client_is_up_to_date() {
        let digest = sha();
        let versions = vec![version(
            "v0.15.0",
            vec![gui_exe(
                "rust-tunnel-client-gui-windows-x86_64.exe",
                Some(&digest),
            )],
        )];
        // 新版本客户端配上旧归档：不能把它「降级」，回显归档里的最新 GUI 版本。
        assert_eq!(
            select_update(Some("0.16.0"), &versions),
            UpdateDecision::UpToDate {
                version: "v0.15.0".into()
            }
        );
    }

    #[test]
    fn missing_sha256_is_skipped() {
        // 没有校验和的产物无法保证替换安全：既不算更新，也不在清单里出现。
        let versions = vec![version(
            "v0.16.0",
            vec![gui_exe("rust-tunnel-client-gui-windows-x86_64.exe", None)],
        )];
        assert_eq!(
            select_update(Some("0.15.0"), &versions),
            UpdateDecision::UpToDate {
                version: "0.15.0".into()
            }
        );

        // 同一版本里一个带校验和、一个不带：只下发带校验和的那个。
        let digest = sha();
        let versions = vec![version(
            "v0.16.0",
            vec![
                gui_exe("rust-tunnel-client-gui-windows-x86_64.exe", Some(&digest)),
                gui_exe("rust-tunnel-client-gui-macos-aarch64", None),
            ],
        )];
        match select_update(Some("0.15.0"), &versions) {
            UpdateDecision::Available { files, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].name, "rust-tunnel-client-gui-windows-x86_64.exe");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn malformed_or_old_client_version_is_unsupported() {
        let digest = sha();
        let versions = vec![version(
            "v0.16.0",
            vec![gui_exe(
                "rust-tunnel-client-gui-windows-x86_64.exe",
                Some(&digest),
            )],
        )];
        assert_eq!(
            select_update(Some("garbage"), &versions),
            UpdateDecision::Unsupported
        );
        assert_eq!(select_update(None, &versions), UpdateDecision::Unsupported);
        // 低于 MIN_UPDATE_CLIENT_VERSION 的客户端不认识这些变体，绝不能下发。
        assert_eq!(
            select_update(Some("0.14.0"), &versions),
            UpdateDecision::Unsupported
        );
    }

    #[test]
    fn empty_archive_echoes_client_version() {
        assert_eq!(
            select_update(Some("0.15.0"), &[]),
            UpdateDecision::UpToDate {
                version: "0.15.0".into()
            }
        );
    }

    #[test]
    fn directly_replaceable_matches_auto_update_naming() {
        assert!(is_directly_replaceable(
            "rust-tunnel-client-gui-windows-x86_64.exe"
        ));
        assert!(is_directly_replaceable(
            "rust-tunnel-client-gui-macos-aarch64"
        ));
        assert!(is_directly_replaceable(
            "rust-tunnel-client-gui-windows-x86_64.EXE"
        ));
        assert!(!is_directly_replaceable(
            "rust-tunnel-client-gui-windows-x86_64.zip"
        ));
        assert!(!is_directly_replaceable(
            "rust-tunnel-client-gui-macos-aarch64.dmg"
        ));
        assert!(!is_directly_replaceable("SHA256SUMS"));
        assert!(!is_directly_replaceable(
            "rust-tunnel-client-service-windows-x86_64.exe"
        ));
    }
}
