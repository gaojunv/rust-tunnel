# macOS 登录窗口只读 agent（P1）

> 目标：机器停在 **登录窗口**（无人登录）时，控制器仍能**看到**画面。
> 登录窗口的键鼠注入在 macOS 上不可行（见 `MACOS-UNATTENDED.md` 的依据），因此本功能
> **只读**：控制器只能发起「仅观看」，完全控制/文件传输在协议层被拒绝。

## 1. 能力与边界

| 能力 | 登录窗口 agent |
|---|---|
| 画面采集 | ✅ ScreenCaptureKit（需 macOS **14.4+**） |
| 鼠标/键盘注入 | ❌ 协议层拒绝「完全控制」；平台层也是空注入器 |
| 文件传输 | ❌ `share_dir=None`，且只读策略拒绝「仅文件传输」 |
| 剪贴板 | ❌ 不提供 |
| 声音 | ❌ `audio_allowed=false` |
| 锁屏命令 | ❌ 返回「登录窗口 agent 不会锁定屏幕」 |
| 登录后的普通会话 | 仍由用户 GUI 承载（本 agent 随 LoginWindow 会话结束退出） |

**前提**：

- 系统 **macOS 14.4+**（ScreenCaptureKit 在 pre-login 上下文可用；14.0–14.3 不支持）。
- 已授予「屏幕录制」。登录窗口下系统可能弹出授权框，但无人值守时没人能点；
  长期无人值守必须申请 Apple 的 **`com.apple.developer.persistent-content-capture`**
  （系统设置里显示为「远程桌面」权限），否则 Sequoia 起会周期性要求重新确认。
- FileVault 开启时，重启后的首次解锁仍需在机器前完成（第三方代码此时根本不会运行）。

## 2. 安装

安装器需要 **root**（`sudo`）：

```bash
# 1) 准备 host.toml（服务端地址 + 客户端 token + 名称 + TLS）
cat > /tmp/host.toml <<'EOF'
server = "example.com:8080"
name = "my-mac"
password = "<client_token>"
tls = true
tls_insecure = false
EOF
chmod 600 /tmp/host.toml

# 2) 安装（把当前 .app 整体复制到 root-only 目录；plist 写到 /Library/LaunchAgents）
sudo /Applications/RustTunnel.app/Contents/MacOS/rust-tunnel-client-gui \
  --install-login-agent --config /tmp/host.toml

# 3) 卸载
sudo /Applications/RustTunnel.app/Contents/MacOS/rust-tunnel-client-gui \
  --uninstall-login-agent
```

安装布局：

```text
/Library/PrivilegedHelperTools/RustTunnelLoginAgent.app   完整 .app（同 bundle id/签名）
/Library/Application Support/rust-tunnel/host.toml        0600 root:wheel
/Library/LaunchAgents/com.rust-tunnel.loginagent.plist    0644 root:wheel
/Library/Logs/rust-tunnel-loginagent.log
```

plist 的关键项：

```xml
<key>LimitLoadToSessionType</key><array><string>LoginWindow</string></array>
<key>RunAtLoad</key><true/>
```

**它只在登录窗口会话启动**：注销/重启后出现；当前已登录的会话不会启动它。
安装后不需要 `launchctl bootstrap`——`/Library/LaunchAgents` 由对应会话的 launchd 加载。

## 3. 行为

- 以 **root** 运行，读取 `/Library/Application Support/rust-tunnel/host.toml`；
  `enable_agent=false`、不加入 mesh、不跑 agent 执行器。
- 用 host.toml 里的 `name` 注册客户端——与登录后的 GUI 是**同一个客户端身份**，Web 面板里
  不会多出一台机器。登录瞬间两个进程可能短暂同时存在，服务端会踢掉旧连接；
  LoginWindow 会话结束后 launchd 终止 agent。
- 接入确认固定为 **Auto**（登录窗口没有弹窗可点），但引擎的 `set_read_only(true)` 只放行
  `RemoteMode::ViewOnly`，所以自动接受的范围被限制为「只读观看」。
- 采集源是 ScreenCaptureKit；`showsCursor=false`，光标形状仍走协议 v9。
- 画面空闲时不编码、5 s 无会话自动停流（与 GUI 被控端同一套采集层）。
- ScreenCaptureKit 不允许时：日志给出中文原因并继续重试，不会静默黑屏。
- 进程随会话结束收到 SIGTERM；不做优雅收尾（SCK/编码器随进程释放）。

## 4. 安全

| 风险 | 对策 |
|---|---|
| 普通用户替换 root 运行的二进制 | helper 复制到 `/Library/PrivilegedHelperTools`，递归 `root:wheel`、目录 0755、文件 0644/0755 |
| host.toml 泄漏客户端 token | 目标目录 `root:wheel 0755`、文件 `0600`；安装时拒绝 world-writable 的源文件 |
| 控制器误以为拿到完全控制 | 引擎在 `RemoteSessionIncoming` 阶段拒绝非 `ViewOnly`，并回明确原因 |
| 剪贴板/文件泄露 | 不安装剪贴板实现、`share_dir=None`、协议层拒绝文件模式 |
| 自动接受扩大攻击面 | 仍需服务端授权 + 一次性 token 的会话鉴权；只读策略独立于审批策略 |

**没有**做的事：不调用 `tccutil`/不修改 TCC 数据库、不尝试绕过 SIP、不注入登录窗口。
这些要么不可能，要么会破坏系统完整性。

## 5. 验证

```bash
# 安装后确认文件与权限
ls -l /Library/LaunchAgents/com.rust-tunnel.loginagent.plist
sudo ls -ld /Library/PrivilegedHelperTools/RustTunnelLoginAgent.app
sudo stat -f '%Su:%Sg %Sp' "/Library/Application Support/rust-tunnel/host.toml"

# 注销到登录窗口后，从 launchd 视角确认作业已加载（在登录窗口下用 SSH 执行）
sudo launchctl print loginwindow/com.rust-tunnel.loginagent

# 日志
sudo tail -f /Library/Logs/rust-tunnel-loginagent.log
```

在登录窗口用控制器连接时**选「仅观看」**；选「完全控制/文件传输」应看到
「被控端处于只读模式（登录窗口），只接受『仅观看』」。

**不注销的冒烟测试**（不等于真实验收）：

```bash
sudo /Library/PrivilegedHelperTools/RustTunnelLoginAgent.app/Contents/MacOS/rust-tunnel-client-gui \
  --login-agent --config "/Library/Application Support/rust-tunnel/host.toml"
```

此时它运行在**当前用户会话**，采集用的是该进程的 TCC 身份；能验证注册、只读拒绝与采集，
但**不能**证明登录窗口会话下的授权与 WindowServer 可达性。

真实验收清单：

- [ ] 注销到登录窗口，控制器能看到画面（ViewOnly）；
- [ ] 完全控制/文件传输被明确拒绝；
- [ ] 登录后 agent 退出、GUI 接管同一个客户端名（Web 面板不出现重复机器）；
- [ ] 重启 + 不自动登录：登录窗口仍可看；
- [ ] 登录窗口下屏幕录制授权状态（`CGPreflightScreenCaptureAccess`）与日志；
- [ ] 开启 FileVault 时明确记录「首次解锁仍需本机操作」的限制。

## 6. 与 `MACOS-UNATTENDED.md` 的关系

`MACOS-UNATTENDED.md` 是完整的可行性论证与业界对比。本功能落地的是其中的 **P1：登录窗口只读**。
未实现的部分：

- 登录窗口输入（平台不可行；可选的 P2 是绕道 macOS 自带屏幕共享的 RFB）；
- `persistent-content-capture` entitlement 申请与 Developer ID 公证 `.pkg`（P3）；
- MDM/PPPC 企业部署（P4）。
