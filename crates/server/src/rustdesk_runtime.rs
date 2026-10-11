//! RustDesk 启动/重启统一入口（hbbs 会合 + hbbr 中继）。
//!
//! 两处调用方共用 `apply_rustdesk_config`：
//! - `src/bin/server.rs` 启动路径
//! - API `POST /api/rustdesk` 更新路径
//!
//! 端口约定（与上游一致）：会合主端口 `rendezvous_port`（TCP 信令与 UDP 心跳同号），
//! NAT 探测端口 = `rendezvous_port - 1`，中继端口 `relay_port`。
//!
//! 端口全部由本模块绑定：`rust-tunnel-rustdesk` crate 只做协议处理，既不 bind
//! 也不登记端口——端口登记是服务端 `ProxyPortsState` 的职责（本模块不登记，
//! RustDesk 端口不参与 SS/Trojan 的端口表）。

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::control_plane::{RustdeskRuntimeStatus, ServerState};
use crate::dynamic_config::RustdeskDynamicConfig;
use crate::stats::EntityType;
use rust_tunnel_rustdesk::key::ServerKey;
use rust_tunnel_rustdesk::peer::PeerRegistry;
use rust_tunnel_rustdesk::relay::{RelayConfig, RelayServer, RelayStats};
use rust_tunnel_rustdesk::rendezvous::{
    Mask, RendezvousConfig, RendezvousServer, RendezvousStats,
};

/// 单个 RustDesk 任务的有界退出等待上限。
///
/// 停旧时逐个 await 任务句柄：正常情况下任务收到 abort 后立即返回（监听套接字随栈帧
/// 析构释放，poller 也跑完幻影连接数释放），此时等待即刻结束；500ms 只是兜底——
/// 万一某任务卡在不可中断段，超时后 abort 走人，不让 API 长时间挂起。
const RUSTDESK_STOP_TIMEOUT: Duration = Duration::from_millis(500);

/// 会合/中继统计轮询间隔。
const RUSTDESK_STATS_INTERVAL: Duration = Duration::from_secs(10);

/// 会合/中继默认监听地址：公网服务器需监听所有网卡。
///
/// 仅作为 `effective_bind_addr` 的默认值（静态配置未给绑定时使用）；实际生效地址
/// 由 `state.proxy_ports.rustdesk_bind_addr` 在启动时注入，纯 IPv6 主机可据此覆盖。
const RUSTDESK_BIND_ADDR: &str = "0.0.0.0";

/// 由 `rendezvous_port` 推导 NAT 探测端口（主端口 - 1）。
///
/// 用 `saturating_sub` 保证 `rendezvous_port = 0/1` 时不 panic（这两个值本身非法，
/// 由 API 侧校验拦下，此处只做下溢兜底）。
#[must_use]
pub fn nat_test_port(rendezvous_port: u16) -> u16 {
    rendezvous_port.saturating_sub(1)
}

/// 把配置里的单台 `host:port` 中继地址包装成下发给客户端的列表。
///
/// 空串 → 空列表，表示「不下发中继地址，由客户端用自己配置的」。
/// **刻意只支持单台中继**：当前部署形态是一台公网服务器承载全部服务，
/// 多中继（客户端轮转选择）不在计划内，故配置面不暴露列表字段。
fn relay_servers_from(relay_server: &str) -> Vec<String> {
    let s = relay_server.trim();
    if s.is_empty() {
        Vec::new()
    } else {
        vec![s.to_string()]
    }
}

/// 四个监听套接字。
struct Listeners {
    /// hbbs 信令 TCP（与心跳共用端口号）。
    signaling_tcp: TcpListener,
    /// hbbs 心跳/推送 UDP（与信令共用端口号）。
    heartbeat_udp: UdpSocket,
    /// NAT 探测 TCP。
    nat_tcp: TcpListener,
    /// hbbr 中继 TCP。
    relay_tcp: TcpListener,
}

/// 取出生效的绑定地址：空串（未配置 / Default）退化为 [`RUSTDESK_BIND_ADDR`]。
///
/// 归一化集中在此处，保证「默认值只定义一次」——`bind_listeners` 不会再各自兜底。
fn effective_bind_addr(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        RUSTDESK_BIND_ADDR
    } else {
        trimmed
    }
}

/// 应用一份 RustDesk 配置：停旧 → 解析密钥/掩码 → 绑定四个监听 → 启动四个任务。
///
/// 停旧采用「触发 abort 并逐个 await 任务句柄」的有界等待策略：句柄返回即证明旧
/// 监听套接字已 drop、旧统计轮询任务已跑完收尾，因此改端口重启不存在「旧端口尚未
/// 释放」的竞态（比固定 sleep 猜测更强）。超时才 abort 兜底。
///
/// # Errors
/// 密钥/掩码解析失败，或任一端口绑定失败时，返回可读的中文错误串，并把同一
/// 消息写入 `rustdesk_runtime.last_error`（`listening` 保持 false）。
pub async fn apply_rustdesk_config(
    state: &ServerState,
    cfg: &RustdeskDynamicConfig,
) -> Result<(), String> {
    // 1. 停掉现有实例：触发 abort 后有界 await 四个任务句柄（会合 TCP/UDP、NAT 探测、
    //    中继、统计轮询）。句柄返回即代表旧 `TcpListener`/`UdpSocket` 已 drop，且旧
    //    poller 已跑完幻影连接数释放——这正是「改端口重启不撞旧端口」的确定性保证。
    stop_existing(state).await;

    if !cfg.enabled {
        *state.proxy_ports.rustdesk_runtime.write().await = RustdeskRuntimeStatus::default();
        return Ok(());
    }

    // 2. 密钥解析（空串 = 不校验 licence_key、不签名）
    let key = match ServerKey::parse(&cfg.key) {
        Ok(k) => k,
        Err(e) => return record_error(state, format!("RustDesk 密钥解析失败: {e}")).await,
    };
    // 运营提示（只进日志，不进 GET 响应——响应契约已冻结）：只粘贴了公钥（有 key 但
    // 无私钥）时无法签名 IdPk，要求签名校验的安全客户端会连不上。
    if !cfg.key.is_empty() && !key.can_sign() {
        warn!(
            "RustDesk 密钥已设置但无签名私钥（可能只粘贴了公钥），需要签名 IdPk 的客户端将无法连接；\
             请改用 POST /api/rustdesk 的 generate_key 生成完整密钥对"
        );
    }

    // 3. 掩码解析（空串 = 不做内网判定，退化为「公网 IP 相同即同内网」）
    let mask = match cfg.mask.trim() {
        "" => None,
        m => match Mask::from_str(m) {
            Ok(mask) => Some(mask),
            Err(e) => return record_error(state, format!("RustDesk 内网掩码解析失败: {e}")).await,
        },
    };

    // 4. 读取生效绑定地址（读锁内 clone 后立即释放），顺序绑定全部监听；
    //    任一失败则已绑定的随 `Listeners` 栈帧析构自动释放
    let bind_addr = {
        let raw = state.proxy_ports.rustdesk_bind_addr.read().await;
        effective_bind_addr(raw.as_str()).to_string()
    };
    let listeners = match bind_listeners(cfg, &bind_addr).await {
        Ok(l) => l,
        Err(e) => return record_error(state, e).await,
    };

    // 5. 编译服务实例一次，四个任务共享同一批 Arc（统计与登记表口径一致）
    let key = Arc::new(key);
    let peers = Arc::new(PeerRegistry::new());
    let rendezvous_cfg = RendezvousConfig {
        // serial = 0：永不下发 ConfigUpdate。服务端列表由管理页/客户端自行配置，
        // 会合服务端不越权推送（刻意的产品决定，不是遗漏）。
        serial: 0,
        rendezvous_servers: Vec::new(),
        relay_servers: relay_servers_from(&cfg.relay_server),
        local_ip: cfg.local_ip.clone(),
        mask,
        always_use_relay: cfg.always_use_relay,
        tcp_idle_timeout: Duration::from_secs(30),
    };
    let rendezvous = Arc::new(RendezvousServer::new(key.clone(), peers.clone(), rendezvous_cfg));
    let relay = Arc::new(RelayServer::new(key, RelayConfig::default()));

    // 6. 统一 abort channel：句柄存 state 供后续 apply/停用，每个任务取一份订阅。
    //
    //    先取出会合侧统计句柄：`spawn_nat_test` 会按值消费 rendezvous Arc，
    //    而统计轮询任务持续读会合/中继两侧的累计计数（⑦/⑧ 的消费口径）。
    let rendezvous_stats = rendezvous.stats();
    let relay_stats = relay.stats();
    let (abort_tx, _) = watch::channel(false);
    *state.proxy_ports.rustdesk_abort.write().await = Some(abort_tx.clone());
    let rendezvous_task = spawn_rendezvous(
        rendezvous.clone(),
        listeners.signaling_tcp,
        listeners.heartbeat_udp,
        &abort_tx,
    );
    let nat_test_task = spawn_nat_test(rendezvous, listeners.nat_tcp, &abort_tx);
    let relay_task = spawn_relay(relay.clone(), listeners.relay_tcp, &abort_tx);
    let stats_task = spawn_stats_poller(
        state.clone(),
        relay_stats.clone(),
        rendezvous_stats.clone(),
        peers.clone(),
        cfg.relay_port,
        &abort_tx,
    );
    // 四个句柄登记到 state：下一次 stop_existing 会逐个有界 await，保证旧监听已释放、
    // 旧 poller 收尾已完成，新一代码才能写状态（幻影连接数不会泄漏）。
    *state.proxy_ports.rustdesk_tasks.write().await =
        vec![rendezvous_task, nat_test_task, relay_task, stats_task];

    // 7. 写入运行状态（含三项瞬时计数与两项累计计数；之后由统计轮询任务持续刷新）
    let relay_snapshot = relay_stats.snapshot();
    let relay_pairs = relay_snapshot.active_pairs;
    let online_peers = peers.online_count().await;
    let known_peers = peers.len().await;
    {
        let mut rt = state.proxy_ports.rustdesk_runtime.write().await;
        *rt = RustdeskRuntimeStatus {
            listening: true,
            rendezvous_port: Some(cfg.rendezvous_port),
            relay_port: Some(cfg.relay_port),
            last_error: None,
            online_peers,
            known_peers,
            relay_pairs,
            // 累计计数在启动瞬间就取当前快照值（全新实例为 0），而不是先写 0
            // 等第一次轮询才补——避免 GET 在启动窗口里读到虚假的 0。
            relay_total_pairs: relay_snapshot.total_pairs,
            rejected_total: rendezvous_stats.snapshot().rejected + relay_snapshot.rejected,
        };
    }
    info!(
        "RustDesk 已启动：hbbs 会合 {}/udp、NAT 探测 {}、hbbr 中继 {}",
        cfg.rendezvous_port,
        nat_test_port(cfg.rendezvous_port),
        cfg.relay_port
    );
    Ok(())
}

/// 停止现有实例：触发 abort，然后有界 await 四个任务句柄（未运行时为 no-op）。
///
/// 锁纪律（防死锁）：先在锁内把 abort 句柄与任务句柄 `take` 出来，**释放锁之后**再
/// `send(true)` 并 await 任务——绝不能在持有异步锁时等待任务，否则任务收尾回写
/// `rustdesk_runtime` 时会与持锁方互等卡死。两把锁的获取顺序固定为
/// 「先 `rustdesk_abort` 再 `rustdesk_tasks`」，避免与将来调用点交叉取锁。
///
/// 等待的价值：poller 收到 abort 后先跑 `release_mirrored_conns` 才 return，await
/// 它的句柄即保证幻影连接数释放（R1）在新一代写状态**之前**完成；监听任务 return
/// 即代表 `TcpListener`/`UdpSocket` 已 drop，端口真正释放。超时则 `abort()` 兜底，
/// 不让 API 因个别任务卡住而长时间挂起。
async fn stop_existing(state: &ServerState) {
    let (abort, tasks) = {
        let mut abort_guard = state.proxy_ports.rustdesk_abort.write().await;
        let abort = abort_guard.take();
        let mut tasks_guard = state.proxy_ports.rustdesk_tasks.write().await;
        let tasks = std::mem::take(&mut *tasks_guard);
        (abort, tasks)
    };
    if let Some(tx) = abort {
        let _ = tx.send(true);
    }
    for mut handle in tasks {
        if tokio::time::timeout(RUSTDESK_STOP_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            // 超时兜底：任务可能卡在不可中断段。abort 后不再 await，否则超时保护失效。
            handle.abort();
        }
    }
    // 给被 abort 的任务一次调度机会，尽量让监听在新一代绑定之前释放。
    tokio::task::yield_now().await;
}

/// 把启动错误写入运行状态，并把同一错误串返回给调用方。
async fn record_error(state: &ServerState, msg: String) -> Result<(), String> {
    {
        let mut rt = state.proxy_ports.rustdesk_runtime.write().await;
        *rt = RustdeskRuntimeStatus {
            last_error: Some(msg.clone()),
            ..RustdeskRuntimeStatus::default()
        };
    }
    Err(msg)
}

/// 绑定四个监听；任一失败返回中文错误串（含端口与 OS 错误）。
///
/// `bind_addr` 为已归一化的生效地址（见 [`effective_bind_addr`]），由启动路径从
/// `state.proxy_ports.rustdesk_bind_addr` 注入，支持纯 IPv6 主机指定具体网卡地址。
async fn bind_listeners(cfg: &RustdeskDynamicConfig, bind_addr: &str) -> Result<Listeners, String> {
    let rendezvous_port = cfg.rendezvous_port;
    let nat_port = nat_test_port(rendezvous_port);
    let relay_port = cfg.relay_port;

    let signaling_tcp = bind_tcp("会合(hbbs) TCP", bind_addr, rendezvous_port).await?;
    let heartbeat_udp = UdpSocket::bind((bind_addr, rendezvous_port))
        .await
        .map_err(|e| bind_err("会合(hbbs) UDP", rendezvous_port, &e))?;
    let nat_tcp = bind_tcp("NAT 探测", bind_addr, nat_port).await?;
    let relay_tcp = bind_tcp("中继(hbbr)", bind_addr, relay_port).await?;
    Ok(Listeners {
        signaling_tcp,
        heartbeat_udp,
        nat_tcp,
        relay_tcp,
    })
}

/// 绑定单个 TCP 监听，失败时给出带端口的中文错误。
async fn bind_tcp(what: &str, bind_addr: &str, port: u16) -> Result<TcpListener, String> {
    TcpListener::bind((bind_addr, port))
        .await
        .map_err(|e| bind_err(what, port, &e))
}

/// 构造绑定失败的中文错误串（含端口与 OS 错误）。
fn bind_err(what: &str, port: u16, e: &std::io::Error) -> String {
    format!("RustDesk {what} 端口 {port} 绑定失败: {e}")
}

/// 启动会合服务器任务（TCP 信令 + UDP 心跳推送，同一端口）。
///
/// 返回任务句柄供 `stop_existing` 有界 await：句柄完成即代表监听套接字已 drop。
fn spawn_rendezvous(
    server: Arc<RendezvousServer>,
    tcp: TcpListener,
    udp: UdpSocket,
    abort_tx: &watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    let udp = Arc::new(udp);
    let abort = abort_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) = server.serve_rendezvous(tcp, udp, abort).await {
            tracing::error!("RustDesk 会合(hbbs)监听错误: {}", e);
        }
    })
}

/// 启动 NAT 探测端口任务（主端口 - 1）。
fn spawn_nat_test(
    server: Arc<RendezvousServer>,
    tcp: TcpListener,
    abort_tx: &watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    let abort = abort_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) = server.serve_nat_test(tcp, abort).await {
            tracing::error!("RustDesk NAT 探测监听错误: {}", e);
        }
    })
}

/// 启动中继服务器任务（hbbr）。
fn spawn_relay(
    server: Arc<RelayServer>,
    tcp: TcpListener,
    abort_tx: &watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    let abort = abort_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) = server.serve(tcp, abort).await {
            tracing::error!("RustDesk 中继(hbbr)监听错误: {}", e);
        }
    })
}

/// 统计轮询任务：每 10s 读取会合/中继快照，把中继的字节与活跃配对数增量写入统一
/// 统计采集器，并刷新 `rustdesk_runtime` 的瞬时计数与累计计数。
///
/// 口径说明：中继的 `bytes_from_first` / `bytes_from_second` 是按两条腿的
/// 「先到方 / 后到方」计的，**不是「客户端→服务端 / 服务端→客户端」的方向**。
/// 这里分别映射到 `bytes_in` / `bytes_out`（first→in、second→out），只是取一个
/// 稳定可分的口径；对一路上下行基本对称的实时桌面流而言，二者之和即总转发量。
///
/// 累计计数口径（只写 `rustdesk_runtime`，不进采集器）：
/// - `relay_total_pairs` = 中继累计配对数 `RelayStats::total_pairs`；
/// - `rejected_total` = 会合 `RendezvousStats::rejected` + 中继 `RelayStats::rejected`
///   之和（两侧都是「凭据不符 / 限流 / 格式非法」被拒的请求）。
///
/// **累计量只读直接赋值，绝不经 `incr_conns`/`decr_conns` 喂采集器**：把 gauge 映射
/// 成 incr/decr 正是幻影连接数的成因（见退出分支注释），而单调递增的累计计数直接
/// 读进响应字段没有任何风险，不必也**不能**走采集器。
///
/// 会合侧 `RendezvousStats` 的 7 项累计计数只打 `tracing::debug!`（GET 响应契约
/// 已冻结，无处安放），并在 rejected 出现增量时 `warn!`，帮助定位拒绝原因。
fn spawn_stats_poller(
    state: ServerState,
    relay_stats: Arc<RelayStats>,
    rendezvous_stats: Arc<RendezvousStats>,
    peers: Arc<PeerRegistry>,
    relay_port: u16,
    abort_tx: &watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    let entity_id = format!("rustdesk:{relay_port}");
    let mut abort = abort_tx.subscribe();
    tokio::spawn(async move {
        // 首帧基线：让第一次增量是「本 tick 内的新增」，而不是自启动以来的累计总量
        let mut prev = relay_stats.snapshot();
        let mut prev_rz = rendezvous_stats.snapshot();
        let mut interval = tokio::time::interval(RUSTDESK_STATS_INTERVAL);
        loop {
            tokio::select! {
                changed = abort.changed() => {
                    if changed.is_err() || *abort.borrow() {
                        // 退出前把本代 poller 镜像进 active_conns 的连接数释放掉。
                        //
                        // 为什么是 prev.active_pairs：本任务每 tick 用 incr/decr 把 relay 的
                        // active_pairs gauge 镜像进统计采集器，净贡献恒等于「最后一次观测到的
                        // gauge 值」；而 prev = now 是在 incr/decr 之后赋值的，所以退出时
                        // prev.active_pairs 正是那个最后观测值。
                        //
                        // 不要改成「退出前重新读一次快照」或改用别的种子：每次 apply 都重建
                        // RelayServer，新一代的快照从 0 起算，重读只会得到 0——重启时的幻影
                        // 连接数就泄漏回来了（decr 只减不清零，get_summary 会把残留计入总数）。
                        release_mirrored_conns(&state, &entity_id, prev.active_pairs);
                        info!("RustDesk 统计轮询任务退出");
                        return;
                    }
                }
                _ = interval.tick() => {
                    let now = relay_stats.snapshot();
                    let rz = rendezvous_stats.snapshot();
                    // 拒绝增量（会合 + 中继两侧之和）必须在 `prev = now` / `prev_rz = rz`
                    // 覆盖基线之前取差值；saturating 兜底防止快照回退时为负。
                    let rendezvous_rejected_delta =
                        rz.rejected.saturating_sub(prev_rz.rejected);
                    let relay_rejected_delta = now.rejected.saturating_sub(prev.rejected);
                    // 字节增量（saturating 兜底：快照理论上单调，仍避免任何回退为负）
                    let bytes_in = now.bytes_from_first.saturating_sub(prev.bytes_from_first);
                    let bytes_out = now.bytes_from_second.saturating_sub(prev.bytes_from_second);
                    state
                        .stats_collector
                        .record_bytes(EntityType::Rustdesk, &entity_id, bytes_in, bytes_out);
                    // 活跃配对数增量：incr/decr 是单步 API，按差值循环加减（数值很小）
                    let active = now.active_pairs;
                    if active > prev.active_pairs {
                        for _ in 0..(active - prev.active_pairs) {
                            state.stats_collector.incr_conns(EntityType::Rustdesk, &entity_id);
                        }
                    } else {
                        for _ in 0..(prev.active_pairs - active) {
                            state.stats_collector.decr_conns(EntityType::Rustdesk, &entity_id);
                        }
                    }
                    prev = now;
                    prev_rz = rz;
                    // 拒绝增量告警：只在 >0 时打，避免刷屏；文案给出可定位的原因。
                    let rejected_delta = rendezvous_rejected_delta + relay_rejected_delta;
                    if rejected_delta > 0 {
                        warn!(
                            "RustDesk 本 tick 拒绝 {rejected_delta} 个请求\
                             （会合 {rendezvous_rejected_delta} / 中继 {relay_rejected_delta}）：\
                             常见原因为 licence_key 不符、per-ID/per-IP 限流或帧格式非法"
                        );
                    }
                    // 会合侧 7 项累计计数只读打一条 debug（GET 响应契约已冻结，不对外暴露）。
                    tracing::debug!(
                        "RustDesk 会合累计: tcp_connections={} udp_datagrams={} heartbeats={} \
                         pk_registrations={} punch_hole_requests={} relay_requests={} rejected={}",
                        rz.tcp_connections,
                        rz.udp_datagrams,
                        rz.heartbeats,
                        rz.pk_registrations,
                        rz.punch_hole_requests,
                        rz.relay_requests,
                        rz.rejected
                    );
                    // 刷新运行状态三项瞬时计数（GET /api/rustdesk 读取）
                    let online = peers.online_count().await;
                    let known = peers.len().await;
                    let mut rt = state.proxy_ports.rustdesk_runtime.write().await;
                    // 代际守卫：只有状态仍属于本代（relay_port 匹配）才写计数。
                    // 仅判 rt.listening 不够——换端口重启后新旧两代 listening 都为 true，
                    // 旧 poller 就会覆盖新一代的计数；relay_port 相等才唯一确定同一代，
                    // 也能挡住 record_error 重置后残留旧 poller 往失败状态上写数的窗口。
                    // 启动时本守卫会跳过 poller 的第一次 tick（step 7 才写 relay_port），
                    // 这没问题：step 7 随后自己算好并写入三项计数。
                    if rt.relay_port == Some(relay_port) {
                        rt.online_peers = online;
                        rt.known_peers = known;
                        rt.relay_pairs = active;
                        // 累计计数：只读直接赋值，不经采集器（理由见函数文档）。
                        rt.relay_total_pairs = now.total_pairs;
                        rt.rejected_total = rz.rejected + now.rejected;
                    }
                }
            }
        }
    })
}

/// 释放本代 poller 镜像出去的连接数（退出时调用）。
///
/// poller 用 incr/decr 把 relay 的 `active_pairs` gauge 镜像进统计采集器的
/// `active_conns`，其净贡献恒等于最后观测到的 gauge 值（见退出分支注释）——所以按
/// 该值逐个 decr 即可回到 0，不会把上一代会话的计数泄漏成永久幻影连接。
fn release_mirrored_conns(state: &ServerState, entity_id: &str, count: usize) {
    for _ in 0..count {
        state
            .stats_collector
            .decr_conns(EntityType::Rustdesk, entity_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> RustdeskDynamicConfig {
        RustdeskDynamicConfig {
            enabled: true,
            rendezvous_port: 21116,
            relay_port: 21117,
            key: String::new(),
            relay_server: String::new(),
            always_use_relay: false,
            local_ip: String::new(),
            mask: String::new(),
        }
    }

    #[test]
    fn nat_test_port_derivation_does_not_underflow() {
        assert_eq!(nat_test_port(21116), 21115);
        assert_eq!(nat_test_port(1), 0);
        assert_eq!(nat_test_port(0), 0);
    }

    #[test]
    fn relay_servers_from_single_or_empty() {
        assert!(relay_servers_from("").is_empty());
        assert!(relay_servers_from("   ").is_empty());
        assert_eq!(
            relay_servers_from("relay.example.com:21117"),
            vec!["relay.example.com:21117".to_string()]
        );
        assert_eq!(relay_servers_from(" relay:1 "), vec!["relay:1".to_string()]);
    }

    /// 密钥持久化 vs 回传的不对称：持久化的是私钥（可签名），回传的是公钥字符串。
    /// 一旦哪天真反过来（存公钥、回传私钥），客户端会拿错串、服务端也签不了名。
    #[test]
    fn generated_key_persists_secret_and_returns_public() {
        let sk = ServerKey::generate();
        let persisted = sk
            .to_secret_base64()
            .expect("generated key must have a secret half");
        let returned = sk.key().to_string();

        let reloaded = ServerKey::parse(&persisted).expect("persisted secret must reparse");
        assert!(reloaded.can_sign(), "persisted value must be able to sign");
        assert_eq!(
            reloaded.key(),
            returned,
            "returned string must be the public key derived from the persisted secret"
        );
    }

    /// 退出时按「最后观测到的 gauge 值」释放，能把镜像出去的连接数精确归零——
    /// 这正是修复重启幻影连接数的算术依据。
    #[test]
    fn release_mirrored_conns_brings_gauge_back_to_zero() {
        let state = ServerState::new();
        let id = "rustdesk:21117";
        // 模拟 poller 每 tick incr，最终镜像出 3 个活跃连接
        for _ in 0..3 {
            state.stats_collector.incr_conns(EntityType::Rustdesk, id);
        }
        assert_eq!(state.stats_collector.get_summary().rustdesk.total_conns, 3);

        // 退出时释放 3 → 归零（而不是泄漏成永久幻影）
        release_mirrored_conns(&state, id, 3);
        assert_eq!(state.stats_collector.get_summary().rustdesk.total_conns, 0);
    }

    /// 停用路径不得绑定任何端口，且要清空运行状态与 abort 句柄。
    #[tokio::test]
    async fn disabled_clears_runtime_and_binds_nothing() {
        let state = ServerState::new();
        // 先塞一点脏状态，验证会被清干净
        {
            let mut rt = state.proxy_ports.rustdesk_runtime.write().await;
            rt.listening = true;
            rt.relay_pairs = 5;
        }
        let cfg = RustdeskDynamicConfig {
            enabled: false,
            ..base_cfg()
        };
        apply_rustdesk_config(&state, &cfg).await.unwrap();

        let rt = state.proxy_ports.rustdesk_runtime.read().await;
        assert!(!rt.listening);
        assert_eq!(rt.rendezvous_port, None);
        assert_eq!(rt.relay_port, None);
        assert_eq!(rt.online_peers, 0);
        assert_eq!(rt.known_peers, 0);
        assert_eq!(rt.relay_pairs, 0);
        assert!(rt.last_error.is_none());
        drop(rt);
        assert!(state.proxy_ports.rustdesk_abort.read().await.is_none());
    }

    /// 非法密钥在绑定之前就返回：不 bind、listening=false、last_error 记录原因。
    /// 若校验顺序反了（先 bind 后解析），这条测试会绑到真实端口而失败。
    #[tokio::test]
    async fn invalid_key_records_error_without_binding() {
        let state = ServerState::new();
        // "AAAA" base64 解出 3 字节：既非 32（公钥）也非 64（私钥），parse 必失败
        let cfg = RustdeskDynamicConfig {
            key: "AAAA".to_string(),
            ..base_cfg()
        };
        let err = apply_rustdesk_config(&state, &cfg).await.unwrap_err();
        assert!(err.contains("密钥解析失败"), "unexpected error: {err}");

        let rt = state.proxy_ports.rustdesk_runtime.read().await;
        assert!(!rt.listening);
        assert!(rt
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("密钥解析失败")));
    }

    /// 非法掩码同样在绑定之前返回。
    #[tokio::test]
    async fn invalid_mask_records_error_without_binding() {
        let state = ServerState::new();
        let cfg = RustdeskDynamicConfig {
            mask: "not-a-cidr".to_string(),
            ..base_cfg()
        };
        let err = apply_rustdesk_config(&state, &cfg).await.unwrap_err();
        assert!(err.contains("掩码解析失败"), "unexpected error: {err}");
        assert!(!state.proxy_ports.rustdesk_runtime.read().await.listening);
    }

    /// 端口 0 = 内核分配临时端口，必然可用，且 `bind_listeners` 不做端口校验
    /// （校验只在 API/merge 层），因此这组配置能在测试里安全地真实绑定。
    fn ephemeral_cfg() -> RustdeskDynamicConfig {
        RustdeskDynamicConfig {
            rendezvous_port: 0,
            relay_port: 0,
            ..base_cfg()
        }
    }

    /// 空串 / 纯空白退化为默认 `0.0.0.0`；其余原样（去首尾空白）。
    #[test]
    fn effective_bind_addr_normalizes_blank_to_default() {
        assert_eq!(effective_bind_addr(""), "0.0.0.0");
        assert_eq!(effective_bind_addr("   "), "0.0.0.0");
        assert_eq!(effective_bind_addr("127.0.0.1"), "127.0.0.1");
        assert_eq!(effective_bind_addr(" 127.0.0.1 "), "127.0.0.1");
    }

    /// 绑定地址真的从 state 流到了 `bind`：`127.0.0.1` 可绑成功，说明不是硬编码
    /// `0.0.0.0`（否则本用例仍会成功，故另配一条不存在地址的失败用例做对照）。
    /// 同时断言全新实例的两项累计计数从 0 起算。
    #[tokio::test]
    async fn bind_addr_from_state_is_consumed() {
        let state = ServerState::new();
        *state.proxy_ports.rustdesk_bind_addr.write().await = "127.0.0.1".to_string();

        apply_rustdesk_config(&state, &ephemeral_cfg())
            .await
            .unwrap();

        {
            let rt = state.proxy_ports.rustdesk_runtime.read().await;
            assert!(rt.listening, "listening to 127.0.0.1 must succeed");
            // 全新实例无对端：两项累计计数初值都是 0
            assert_eq!(rt.relay_total_pairs, 0);
            assert_eq!(rt.rejected_total, 0);
        }

        // 收停：四个任务句柄已登记，停用后被 drain 并 await 干净
        assert_eq!(state.proxy_ports.rustdesk_tasks.read().await.len(), 4);
        let disabled = RustdeskDynamicConfig {
            enabled: false,
            ..ephemeral_cfg()
        };
        apply_rustdesk_config(&state, &disabled).await.unwrap();
        assert!(state.proxy_ports.rustdesk_tasks.read().await.is_empty());
    }

    /// 本机不存在的地址绑定必失败：证明 state 里的地址确实被消费（若仍硬编码
    /// `0.0.0.0`，这条会「成功」而失败断言不成立）。
    #[tokio::test]
    async fn invalid_bind_addr_records_error() {
        let state = ServerState::new();
        *state.proxy_ports.rustdesk_bind_addr.write().await = "1.2.3.4".to_string();

        let err = apply_rustdesk_config(&state, &ephemeral_cfg())
            .await
            .unwrap_err();
        assert!(err.contains("绑定失败"), "unexpected error: {err}");

        let rt = state.proxy_ports.rustdesk_runtime.read().await;
        assert!(!rt.listening);
        assert!(rt
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("绑定失败")));
    }
}
