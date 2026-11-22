# macOS 无人值守被控端可行性研究

> 范围：macOS 14（Sonoma）及以上，第三方（非 MDM 强制、非 Apple 私有 entitlement 持有者）的 Developer ID 应用。
> 目标：判断「登录界面（loginwindow）无人值守远程控制」在 macOS 上到底能做到什么程度，并给出可落地的分阶段架构。
> 状态：研究 / 设计文档，**不含实现**。日期：2026-11-21。

**证据等级标记**（全文使用）：
- **A**：Apple 官方文档 / Apple DTS 工程师在开发者论坛的明确答复。
- **B**：厂商官方文档（AnyDesk / TeamViewer / Jump Desktop / Screens 等）。
- **C**：第三方实测或社区报告（含 Apple 开发者论坛的用户帖、开源项目实测日志）。
- **D**：推断（由 A/B/C 推导，**未经验证**，文中会显式标注）。

---

## 0. 结论摘要

**能做到的：**

1. **锁屏（用户已登录、屏幕锁定）**：采集与键鼠注入都可以，且这是 macOS 上最可靠的「无人值守」形态。会话仍在用户域，agent 以该用户身份运行，TCC 授权也在用户域内完成。第三方实测：锁屏画面可采集，键入的密码能在锁屏密码框里出现（C）。
2. **登录窗口（logout 后、无人登录）的画面采集**：**可以**，但前提是两件事同时成立：
   - 采集代码跑在 **LoginWindow 会话的 LaunchAgent** 里，**不是** root LaunchDaemon；
   - 系统是 **macOS 14.4+**（14.4 修复了 ScreenCaptureKit 在 pre-login 上下文的 bug），并使用 Screen Recording TCC 授权。
   Apple DTS 原话：「set your agent's `LimitLoadToSession` property to an array value containing both `Aqua` and `LoginWindow`」「I've recently prototyped this approach using ScreenCaptureKit and it works as expected on macOS 14.4 and later. Prior to that, you have to deploy older technologies … like `CGDisplayStream`」（A）。
3. **登录后的普通会话**：与现有远程控制引擎一致，完全可控。

**不能做到（作为产品承诺）的：**

1. **root LaunchDaemon 直接采集登录窗口**：**不可行**。daemon 在 global/system session，没有 WindowServer 连接；`CGSessionCopyCurrentDictionary` 返回空、`SCShareableContent` 超时/失败（C，且与 Apple 的 daemon/agent 架构说明一致，A）。
2. **登录窗口的键鼠注入**：**不可靠，按「不可行」处理**。
   - 没有公开文档或 API 说明登录窗口支持合成事件；Apple DTS 也未给出任何可用路径。
   - 2016 与 2023 的开发者论坛帖均报告：登录窗口下 `CGEventPost` 的按键被忽略，`CGDisplayStream` 返回 nil（C）。
   - 第三方 2026 实测：某些版本（macOS 26.6）能用 HID tap 打进密码框，但 **26.6.2 起登录窗口不再接受合成输入**，LoginWindow agent 与 root daemon 都一样；鼠标也不动（C）。
   - 结论：**登录窗口「可看不可控」是常态**；「可输入」只能作为实验特性/兼容路径，不能写进 SLA。
3. **FileVault 开启时的开机解锁界面**：**完全不可行**。FileVault 预启动环境在 macOS 启动之前，第三方代码根本不会运行；自动登录也会被 FileVault 禁用（A/B）。

**关键前提（否则「无人值守」会在一个月内断掉）：**

- **`com.apple.developer.persistent-content-capture`**（Apple 限制性 entitlement，需提交申请表审批）。它显示为系统设置里的「远程桌面 / Remote Desktop」权限，作用是**免除 Sequoia 起对屏幕录制权限的周期性（周/月）重新确认**。Jump Desktop Connect 已拿到并在官方文档中说明（B）；Apple 官方文档明确「需先提交申请表并获得 Apple 许可」（A）。
- 没有这个 entitlement 时：登录窗口的屏幕录制确认弹窗**会出现在登录窗口上**，而无人值守时没有人能点它（C）。MDM 的 PPPC **不能**授予 Screen Recording，只能拒绝（A）。

**推荐路线（详见第 3 节）：**

- **MVP（P0）**：root LaunchDaemon + 每会话 agent；先把**锁屏可控**做扎实（这一项在 macOS 上完全可交付）。
- **P1**：新增 **LoginWindow agent，只做画面采集**（14.4+），登录瞬间交接给 Aqua agent；UI 明确提示「登录窗口只读」。
- **P2（可选）**：登录窗口输入绕道 **macOS 自带「屏幕共享」的 RFB**（`screensharingd` 接受合成输入），由操作者在连接时输入 Mac 账户密码；这条路径第三方实测可行（C），但工程量大、需安全评审。
- **P3**：申请 `persistent-content-capture`，做 Developer ID + 公证 `.pkg` 分发；这时才敢承诺「长期无人值守」。
- **部署建议**：真正要求「重启后自动恢复」的客户，推荐 **关闭 FileVault + 自动登录 + 不锁屏/不睡眠**（A/B，安全代价必须书面告知）；或者保持登录只锁屏。

---

## 1. 逐问题分析

### 1.1 会话模型：daemon 为什么够不着登录窗口（对应问题 2）

#### 1.1.1 两种 launchd 作业

- **LaunchDaemon**（`/Library/LaunchDaemons`）永远加载到 **global/system session**，`LimitLoadToSessionType` 对它**无效**（A，Apple DTS 在 thread 814152 的原话）。
- **LaunchAgent**（`/Library/LaunchAgents`）按 `LimitLoadToSessionType` 在**匹配的会话**里各起一个实例；`Aqua`（默认）是用户 GUI 登录后，`LoginWindow` 是登录窗口显示时，且 **LoginWindow agent 以 root 运行**（A，`launchctl(1)` man page：「LoginWindow agents are loaded when the LoginWindow UI is displaying and currently run as root」）。

这意味着一个 plist 写成：

```xml
<key>LimitLoadToSessionType</key>
<array>
  <string>Aqua</string>
  <string>LoginWindow</string>
</array>
```

会在**用户会话**与**登录窗口会话**各起一个进程；两者是不同的 bootstrap namespace、不同的审计会话，**不能共享内存**，只能走 IPC（A，TN2083；DTS thread 674132 里有 LoginWindow 实例在用户登录后仍以 root 残留、出现双进程的实际案例）。

#### 1.1.2 WindowServer 隔离

- 只有 GUI 会话里的进程能连 WindowServer。root daemon 在系统域，`CGSessionCopyCurrentDictionary()` 返回空，`SCShareableContent` 调用超时（C，逐字日志见 aetherdesk issue #3）。
- 因此**任何 GUI 采集/注入代码都必须放进对应会话的 agent**。Apple DTS 的标准建议就是「daemon 做网络与全局事务，agent 做 GUI（录屏、注入），两者用 XPC/IPC 连」（A）。

#### 1.1.3 正规路径与「绕过」

| 手段 | 结论 | 依据 |
|---|---|---|
| `/Library/LaunchAgents` + `LimitLoadToSessionType=[Aqua,LoginWindow]` | **正规且唯一被 Apple 认可的登录窗口 GUI 路径** | A（DTS 814152；launchctl man） |
| `launchctl asuser <uid> …` | 只能「加入已存在的用户 bootstrap」（Mach bootstrap、异常服务器、审计会话），**不能创建会话**；无用户会话时无对象可加 | A（launchctl man） |
| `SMAppService.agent(...)` | **不能注册 LoginWindow agent**；DTS 明确「SMAppService API wasn’t designed to do this」，报了 bug 也只能继续用 `/Library/LaunchAgents` | A（DTS 726864） |
| `LoginHook`（`com.apple.loginwindow LoginHook`） | **已废弃**；以 root 运行、只能装一个、Apple 明确「application developers should not use them in released software」 | A（Apple 归档文档 Customizing Login and Logout） |
| `loginwindow.plist` | 只是偏好设置载体（`autoLoginUser` 等），不是受支持的启动机制 | A/C |
| 关闭 SIP 改系统 TCC.db | 技术上可行但**不可接受**（破坏系统完整性、无法作为产品） | A（DTS 730043 说明系统 TCC.db 受 rootless/SIP 保护） |

> **对「SIP 怎么限制」的回答**：SIP 并不阻止 root 在 `/Library` 安装 daemon/agent，也不阻止 daemon 联网；它保护的是 `/System`、系统 TCC.db、以及部分受保护路径。真正拦住第三方的是 **WindowServer 会话隔离 + TCC + Apple 对 pre-login 注入的不支持**，不是 SIP 本身（A/D）。

#### 1.1.4 一个容易忽略的细节

LoginWindow agent 与 Aqua agent 是**两个进程、两套权限**：

- LoginWindow 实例 **root**，但**没有用户的 Keychain、没有用户的偏好、HOME 是 `/var/root`**；
- Aqua 实例是**普通用户**，能访问用户 Keychain/剪贴板，但**够不着登录窗口**；
- 用户登录的瞬间，LoginWindow 会话结束，launchd 会回收 LoginWindow 实例（第三方实测约 2 秒内被杀，C）。所以**必须设计「交接」**：daemon 保留控制通道，把数据面的采集源从 LoginWindow agent 切到 Aqua agent。

---

### 1.2 登录窗口采集（对应问题 1 的采集部分）

#### 1.2.1 ScreenCaptureKit（SCK）

- **可用版本（登录窗口 / pre-login）：macOS 14.4+**。Apple DTS 明确说 14.4 修复了「SCK 无法在 pre-login 上下文使用」的 bug（radar r.121253782），并在 14.4+ 上原型验证通过（A）。普通 Aqua 会话的 SCK 从 macOS 12.3 起就可用。
- **运行位置**：LoginWindow LaunchAgent（root）。root LaunchDaemon 里不行（A/C）。
- **授权**：走 Screen Recording TCC（`kTCCServiceScreenCapture`）。登录窗口下这个确认**可以弹出来**（第三方实测看到「bypass the private window picker」提示，C），但无人值守时没人点；这正是需要 `persistent-content-capture` 的原因。
- **打包**：应做成带 bundle id 的 `.app`/签名的可执行体，TCC 才能稳定识别（A：TCC 依赖 designated requirement；见 1.4）。

> 实现细节（D）：agent 需要在登录窗口会话里创建 `SCShareableContent`、按 display 建 `SCStream`；音频在登录窗口没有意义，可以先不做。

#### 1.2.2 CGDisplayStream（已废弃，但 DTS 点名可在 <14.4 用）

- Apple DTS 原话：14.4 之前「you have to deploy older technologies in order to work in the pre-login context (like `CGDisplayStream`)」（A）。
- Apple 文档把 `CGDisplayStream` 标为 deprecated（A）。
- 第三方 2023 年实测：登录窗口下 `CGDisplayStream` 初始化返回 nil，时好时坏（Intel 机器更差）（C）。
- 结论：**只作为 14.0–14.3 的兜底**，不作为主路径；建议产品最低要求直接定 **macOS 14.4**。

#### 1.2.3 CGWindowListCreateImage

- Apple 已标 deprecated（A）。
- 没有 Apple 文档说明它支持 pre-login；第三方报告在登录窗口拿不到窗口列表/图像（C/D）。
- 结论：**不作为登录窗口路径**；GUI 会话内也只适合一次性截图，不适合持续串流。

#### 1.2.4 是否需要私有 entitlement 才能「采集」

- **不需要**私有 entitlement 就能采集；需要的是 **Screen Recording TCC**（A）。
- `com.apple.developer.persistent-content-capture` 解决的是「无人值守时的持续性」：Sequoia 起系统会周期性（周/月，随版本调整）要求重新确认屏幕录制，登录窗口上没人能确认（A/B/C）。
- `com.apple.private.screensharing` 之类的 Apple 私有 entitlement 第三方拿不到，**不要考虑**（D）。

---

### 1.3 登录窗口注入（对应问题 1 的注入部分）

#### 1.3.1 可用 API 与它们的真实边界

| 方式 | 需要什么 | 登录窗口实测 | 结论 |
|---|---|---|---|
| `CGEventPost(kCGHIDEventTap, …)` | Accessibility（`kTCCServiceAccessibility`）/ PostEvent TCC | 2016、2023 论坛：按键被忽略；2026 实测：26.6 可打进密码框，26.6.2 起不再到达（C） | **不可依赖** |
| `CGEventPost(kCGSessionEventTap, …)` | 同上 + 有效 GUI 会话 | 登录窗口不是普通用户会话，事件被丢弃（C/D） | 不可依赖 |
| `CGEventPostToPid(loginwindow_pid, …)` | Accessibility | 无公开证据表明 loginwindow 接受定向事件；密码框有 Secure Event Input，WindowServer 会按发送方信任度过滤（D） | **推断为无效，需真机验证** |
| `IOHIDEvent` / `IOHIDPostEvent`（私有） | 私有 API / entitlement | 无公开资料；可能被 SIP/WindowServer 拒绝；App Store 明确禁止 | **不采用** |

#### 1.3.2 「无用户会话时 TCC Accessibility 是什么语义」

- TCC 的授权记录绑定**被授权代码的 designated requirement** + **用户域**；Apple 未公开登录窗口（uid 0、无 Aqua 用户）下具体读哪一份 TCC 库（D）。
- 第三方实测显示：登录窗口下仍会做 TCC 评估（会出现屏幕录制确认框），说明 TCC 在登录窗口是「活着」的，只是**没有人点**（C）。
- MDM 的 PPPC 可以在**受监管（supervised）**设备上预授予 **Accessibility** 与 **PostEvent**，但 **Screen Recording 只能拒绝、不能授予**（A，Apple 部署指南 + Apple 论坛 thread 122414）。
- 因此即使 PPPC 预授予了 Accessibility，**也不能保证**登录窗口的 `CGEventPost` 被 WindowServer 接受——实测表明过滤点可能在 WindowServer 的「发送方信任」而不是 TCC（C/D）。

#### 1.3.3 判定

**登录窗口注入 = 平台不提供、实测不稳定、不能承诺。** 任何「macOS 支持登录界面远程输入」的产品说法，要么依赖 Apple 自带屏幕共享（见第 2 节业界对比），要么依赖自动登录/不锁屏，要么只在特定 macOS 版本上碰巧可用。

> 补充：**锁屏**（用户已登录、屏幕锁定）与**登录窗口**是两回事。锁屏属于用户会话，agent 以用户身份运行，采集与注入都正常（C）。因此「远程解锁」是能做的，「远程登录」不能。

---

### 1.4 授权、签名与 MDM（对应问题 1 的 entitlement、问题 4 的 MDM 部分）

#### 1.4.1 TCC 服务与可授予性

| TCC 服务 | 用途 | 能否用 MDM PPPC 预授予 | 说明 |
|---|---|---|---|
| `kTCCServiceScreenCapture`（Screen Recording） | 采集屏幕 | **不能**，只能 deny | Apple 部署指南：「Deny specified apps access to capture (read) the contents of the system display.」（A） |
| `kTCCServiceAccessibility`（Accessibility） | 控制电脑（注入） | **可以**（需 supervised + MDM） | Apple 部署指南：「Allows specified apps to control the Mac via Accessibility APIs.」（A） |
| `kTCCServicePostEvent`（Post Event） | 向系统事件流发送 CGEvent | **可以**（需 supervised + MDM） | Apple 部署指南：「Allows specified apps to use CoreGraphics APIs to send CGEvents…」（A） |
| `kTCCServiceListenEvent`（Input Monitoring） | 监听键鼠 | 不能授予，只能 deny | 与「注入」无关，不要混淆（A） |
| Remote Desktop（`persistent-content-capture`） | 无人值守持续采集 | **不是 PPPC**，是 entitlement + 用户授权 | 显示为「远程桌面」权限（A/B） |

PPPC 载荷本身：`com.apple.TCC.configuration-profile-policy`，**supervision is required**，enrollment 必须是 Device Enrollment / Automated Device Enrollment（A）。

> 版本备注：Apple 已把 PPPC 的这些服务键在 **macOS 27** 标记为 deprecated，改用声明式 `com.apple.configuration.app.settings` 的 `Privacy` 键（A）。对本项目「最低 macOS 14」没有影响，但企业版实现时应同时支持两套。

#### 1.4.2 tccutil 不能「脚本授权」

- `tccutil` 只有 `reset`：把决定清掉让 App 下次重新弹窗；**没有 grant 子命令**（A，`tccutil(1)` 摘要）。
- 系统 TCC.db 在 `/Library/Application Support/com.apple.TCC/TCC.db`，**受 SIP/rootless 保护**，root 也不能直接改；用户库 TCC.db 的修改对这些服务**不生效**（A，DTS 730043 原话）。
- 结论：**不存在「脚本一键授权 Screen Recording」的正规办法**；只能：人工点一次 + entitlement，或 MDM（且 MDM 也授不了 Screen Recording）。

#### 1.4.3 代码签名与 TCC 稳定性

- TCC 用 **designated requirement** 识别 App；签名身份不稳定（ad-hoc 与 Developer ID 混用、bundle id 变来变去）会导致授权「漂移」、反复失效（A，DTS 730043 + TN3127）。
- 产品要求：**固定 Team ID + 固定 bundle id + Developer ID 签名 + 公证**；更新时保持同一 designated requirement，TCC 授权才不会每次升级都要重授（A/D）。

#### 1.4.4 `persistent-content-capture` 的申请与限制

- Apple 文档：Boolean entitlement，「A Boolean value that indicates whether a Virtual Network Computing (VNC) app needs persistent access to screen capture」；**必须先提交申请表获得 Apple 许可**，再加到 provisioning profile（A）。
- 申请表：`https://developer.apple.com/contact/request/persistent-content-capture/`（A）。
- Jump Desktop Connect 6.10.33 已包含该 entitlement，官方说明它适用于 macOS 14/15+，授权后系统不再每月要求重新确认（B）。
- 推断（D）：该 entitlement 只解决**采集持续性**，不解决注入；也不改变 WindowServer 对登录窗口合成输入的策略。

---

### 1.5 FileVault、自动登录、睡眠（对应问题 4 的替代方案）

#### 1.5.1 FileVault

- FileVault 开启时，开机先进入**预启动解锁界面**，它在 macOS 启动之前；任何第三方 daemon/agent 都还没有运行（A/B）。
- Apple 官方：**FileVault 打开时「自动登录」不可用**（A，support.apple.com/102316）。
- `sudo fdesetup authrestart` 可以让**这一次**重启跳过 FileVault 解锁直接进 macOS，但：
  - 只对这次由 `fdesetup` 触发的重启有效；
  - 断电/崩溃/手动重启仍会回到 FileVault 登录；
  - 命令需要管理员凭据（B，Macworld）。
- 结论：**FileVault 开启 = 重启后必然需要人（或 Apple 自有机制）解锁**。这不是我们能绕的。

#### 1.5.2 自动登录

- 关闭 FileVault 后，可在系统设置里设「自动以此用户登录」（A，102316）。
- 自动登录 + **不锁屏**（「需要密码」设为「永不」/关闭屏保锁定）→ 机器始终有一个 Aqua 会话，agent 始终可采集可注入；掉电重启也能自动恢复（若开启「断电后自动启动」）。
- 这是目前**唯一不依赖登录窗口能力的真无人值守形态**；代价是物理安全（磁盘未加密 + 永久登录），必须让用户书面确认（D，安全常识）。

#### 1.5.3 睡眠/显示器睡眠

- 显示器睡眠后 SCK 会拿不到 display（第三方实测：锁屏约 60 秒后无 display，C）。
- 需要 `pmset` 禁用睡眠/显示器睡眠（`sudo pmset -a sleep 0 displaysleep 0 disksleep 0`），或在 daemon 里常驻 `caffeinate -dims`（A 级命令文档，B/C 级用法）。
- 笔电合盖仍会睡眠，需 `disablesleep` 或外接显示器/电源策略；作为部署限制写明（D）。

### 1.6 替代方案与代价（对应问题 4）

| 方案 | 覆盖场景 | 代价 / 风险 | 建议 |
|---|---|---|---|
| **A. 保持登录 + 只锁屏** | 锁屏可控、可解锁；重启后仍需人（FileVault）或自动登录 | 需要用户会话常驻；显示器不能睡 | **MVP 必做**，最稳 |
| **B. 自动登录 + 不锁屏 + 禁睡眠** | 真无人值守（含掉电重启，若 FileVault 关闭） | 磁盘未加密、物理接触即全权限；不符合高安全场景 | 作为「运维型部署」推荐，书面告知风险 |
| **C. FileVault + `fdesetup authrestart`** | 计划内重启可自动回到登录窗口 | 只对这次重启有效；断电/崩溃无效；需要管理员密码 | 仅作辅助 |
| **D. LoginWindow agent 只读** | 重启/登出后至少能看到登录界面、确认机器状态、做诊断 | 不能输入；需要 14.4+；TCC + entitlement 才稳 | **P1 交付**，UI 明确「请在本机登录」 |
| **E. 登录窗口输入绕道 macOS 屏幕共享（RFB）** | 登录窗口可输入、可完成登录 | 需启用系统「屏幕共享/远程管理」；需实现 RFB 客户端（ARD 认证 type 30，DH + 128-bit AES）；操作者要输入 Mac 账户密码；端口/安全面扩大 | **P2 可选**，默认关闭，企业/高级用户开启 |
| **F. MDM / PPPC（仅企业）** | 预授 Accessibility/PostEvent；批量配置；开启屏幕共享 | 需 supervised + DEP/ADE；**授不了 Screen Recording**；对非受管设备无效 | 企业版增强，不作为通用前提 |
| **G. 让用户在现场登录** | 最诚实 | 不满足「无人值守」 | 作为兜底文案 |

**关于 E 的补充（RFB 路径）：**

- macOS 自带 `screensharingd` 在登录窗口接受合成输入，这是第三方实测结论（C，aetherdesk issue #72：用 ARD 认证 type 30 在登录窗口输入密码并完成登录）。
- RFB 的 security type：**30 = Apple Remote Desktop（DH 密钥交换，使用 Mac 账户密码）**；**2 = VNC 密码**（需在「远程管理 → 计算机设置 → VNC  viewers may control screen with password」里单独设置）。第三方客户端要处理「type 30 被拒后回落 type 2」的细节（C，Zenn 实测文）。
- Apple 官方文档说明 Remote Desktop 的 Control/Observe 在 DH 认证后使用 **128-bit AES**（A）。自带 Screen Sharing 是否完全同构未文档化（D），但第三方实现普遍按 ARD 处理。
- 安全要求：**不要在我们的服务端保存用户密码**；密码只由操作者在会话建立时输入，经我们已有的加密隧道到本机 loopback 的 5900，再由 RFB 认证。RFB 自身是否全程加密需在实现时验证（D）。

---

## 2. 业界对比（对应问题 3）

> 说明：厂商很少公开实现细节；「机制」一列中标注 D 的是推断。

| 产品 | 登录窗口画面 | 登录窗口输入 | 机制 / 依赖 | 依据 |
|---|---|---|---|---|
| **RustDesk**（开源） | **不支持**（登录窗口） | 不支持 | macOS 端有 root service 与 agent，但维护者与社区讨论表明：只能在「已登录用户的锁屏」上工作；logout 后的登录窗口连不上。root service 单独跑也无法注册/连接 | C（discussions #7762） |
| **AnyDesk** | **支持**（官方称可连接「locked or logged-out devices」，Windows/macOS） | 支持（社区实测登录窗口可用） | 开机只有 `AnyDesk --service`（root）一个进程；推测是 root daemon + LoginWindow agent + entitlement（D） | B（unattended access 文档）+ C（RustDesk 讨论里的对照观察） |
| **TeamViewer** | 支持（Host），但历史上长期有「登录窗口卡住/转圈」问题 | 支持（不稳定） | TeamViewer Host 以系统组件方式安装，需要 Screen Recording + Accessibility；社区多次建议改用 macOS 自带屏幕共享兜底 | B/C（社区 thread 69116） |
| **Jump Desktop Connect** | 支持 | 支持（含 Passwordless「仅当无人登录时」） | **已持有 `persistent-content-capture`**（显示为「远程桌面」权限），官方称可消除每月重新授权；Connect 在登录前即可运行（后台项） | B（Sequoia 政策文、Passwordless Logins、reboot 文） |
| **Screens（Edovia）** | 支持 | 支持 | **App Store 应用，自己不做采集/注入**：依赖 macOS 自带「屏幕共享 / 远程管理」服务，自己只做可达性；登录窗口行为由 Apple 的服务决定 | B（Edovia 支持文档）+ C（aetherdesk 的对照结论） |
| **Apple 自带屏幕共享 / 远程管理** | **支持**（Apple 自己的 daemon，有私有权限） | **支持** | 机制未公开（Apple 未文档化登录窗口实现）；第三方无法复制其权限，但可以**作为 VNC 客户端连它** | B/C（Edovia 文档：连接时若登录窗口可见，断开后恢复登录窗口状态） |
| **Microsoft Remote Desktop（macOS 被控）** | 不适用 | 不适用 | macOS 侧没有 RDP 服务端；不构成参考 | — |

**业界结论：**

1. 真正能做到「登录窗口可看可控」的第三方产品，几乎都同时具备：**root 系统组件 + LoginWindow agent + 长期 TCC 授权（多数已拿 entitlement）**；Screens 则干脆把采集/注入外包给 Apple。
2. **没有任何厂商公开宣称「不依赖任何 Apple 特殊授权、纯第三方 API 就能在登录窗口注入」**；论坛与实测也支持「注入受平台限制」的判断。
3. 因此 rust-tunnel 若不想走「Apple 自带屏幕共享」路线，就必须接受「登录窗口只读」这一平台限制，或申请 entitlement 并承担其审核不确定性。

---

## 3. 推荐架构（对应问题 6）

> 与 `MACOS-REMOTE.md`（本分支的阶段 A–C）的关系：采集/编码/音频直接复用
> `crates/mac-capture`（ScreenCaptureKit）、`crates/mac-video`（VideoToolbox）、
> `crates/mac-audio`；本文的 P0–P3 就是那份文档「阶段 D（无人值守）」的拆分。
> 差别只在**进程与会话归属**：Aqua 会话说到底仍是普通用户 agent，而登录窗口需要
> 一个**单独的 LoginWindow agent 进程**，不能把 daemon 或 Aqua agent 直接复用过去。

### 3.1 组件与信任边界

```
┌───────────────────────────── macOS ─────────────────────────────┐
│  root LaunchDaemon (com.rusttunnel.service)                      │
│  - 唯一的客户端身份：控制通道注册、心跳、RemoteSession 信令       │
│  - 配置/令牌（/Library/Application Support/rust-tunnel/host.toml）│
│  - 更新、监督、日志                                             │
│  - 不碰 GUI（不创建 SCStream、不 CGEventPost）                   │
│         ▲ Unix domain socket（peer uid + 代码签名校验）           │
│         │                                                       │
│  ┌──────┴──────────────┐          ┌──────────────────────────┐  │
│  │ LoginWindow agent   │          │ Aqua agent (per user)    │  │
│  │ (root, 登录窗口会话) │          │ (用户会话, 锁屏/桌面)     │  │
│  │ - mac-capture (SCK) │          │ - mac-capture / mac-video │  │
│  │ - 输入：默认禁用     │          │ - 键鼠注入、剪贴板、音频   │  │
│  │ - 可选：RFB 输入桥   │          │ - 编码（硬件优先）         │  │
│  └─────────────────────┘          └──────────────────────────┘  │
│        ▲ 登录瞬间交接（launchd 回收 LW agent）                    │
└──────────────────────────────────────────────────────────────────┘
```

设计要点：

1. **身份唯一**：只有 daemon 注册客户端身份，避免 Windows 上「服务与 GUI 同名互踢」的问题；agent 不直接连服务端，只连本机 daemon。
2. **数据面归属**：建议 daemon 持有控制通道与传输层（TCP/UDP/TLS），agent 只负责采集/编码/注入；帧经本机 Unix socket 传回 daemon。这与现有 `SessionCodec` 的分层一致，也避免两套身份。
   - 备选（更接近 Windows 的「实例」）：由 agent 跑完整会话引擎，daemon 只做监督与拉起；成本是登录交接时要重建会话/重连。**推荐前者**（D，工程判断）。
3. **交接状态机**：`LoginWindowActive → (user logged in) → LoginWindowExiting → AquaStarting → AquaActive`。daemon 在交接窗口内保持控制通道，缓存最多 N 帧/拒绝新会话，避免控制器看到「掉线」。
4. **权限最小化**：daemon 以 root 运行但不加载 GUI 框架；agent 只在需要时请求 TCC。LoginWindow agent 不读用户 Keychain。
5. **IPC 安全**：Unix socket 放在 root-only 目录（`0700`），accept 后取 `getpeereid` 校验 uid，再用 `SecCodeCopyGuestWithAttributes` / `SecStaticCodeCheckValidity` 校验对端 Team ID + designated requirement；拒绝其他进程。

### 3.2 分阶段

#### P0 — 锁屏可控（可交付、无平台赌注）

范围：

- root LaunchDaemon + Aqua agent（`/Library/LaunchAgents`，`LimitLoadToSessionType=Aqua`）。
- 采集：SCK（Aqua 会话 macOS 12.3+ 即可；本产品最低 14）；输入：`CGEventPost`（HID tap）+ Accessibility 授权。
- 覆盖：已登录用户的锁屏/桌面；远程解锁（键入锁屏密码）。
- 不含：登录窗口。

验收：

1. 用户在机器前登录一次、授予 Screen Recording + Accessibility；锁屏后控制器能看画面、能键入密码解锁。
2. 重启且 FileVault 关闭/自动登录时，agent 自动起来，锁屏可控。
3. 显示器不睡眠（pmset/caffeinate 生效），连续 24h 采集不断流。
4. 采集/注入在 1080p/4K 下的帧率与延迟达到现有引擎基线。

#### P1 — 登录窗口只读

范围：

- 新增 `/Library/LaunchAgents/com.rusttunnel.agent.plist`，`LimitLoadToSessionType=[Aqua,LoginWindow]`（或两个 plist，便于分别传参）。
- LoginWindow agent：SCK 采集（14.4+），**不注入**；通过 IPC 把帧交给 daemon。
- LoginWindow agent 应**避免依赖 AppKit/NSApplication**（登录窗口会话没有常规 App 生命周期），只链接 Foundation/CoreGraphics/ScreenCaptureKit，并在真机上验证（D）。同一个 agent 可执行体可由两个 plist/两个 label 分别以 `Aqua` 与 `LoginWindow` 启动。
- UI：会话窗口显示「登录窗口（只读）」横幅，提示在本机登录或启用「屏幕共享输入」（若 P2 开启）。
- 交接：登录后 2s 内切到 Aqua agent，控制器自动恢复。

验收：

1. logout 后在登录窗口能看到画面（含 FileVault 关闭、无用户会话）。
2. 登录瞬间画面不黑屏超过 3s；控制器不需要重新发起会话（或最多一次自动重连）。
3. 登录窗口下发送键鼠事件被本地丢弃并记日志，不发往系统（避免「看起来能点其实无效」）。
4. macOS 14.0–14.3 明确提示「不支持登录窗口采集，请升级到 14.4+」。

#### P2 — 登录窗口输入（可选，默认关闭）

两条互斥实现，建议先做 E1 评估：

- **E1：RFB 输入桥**（推荐先验证）：daemon 在本机 `127.0.0.1:5900` 建立到 `screensharingd` 的连接（需要系统「屏幕共享/远程管理」已启用），把 RFB 的输入通道复用；画面仍用我们自己的 SCK。认证 type 30 使用操作者输入的 Mac 账户密码；不落盘。
- **E2：等待 Apple 提供公开 API**：目前不存在；不要做私有 API 注入。

验收：

1. 登录窗口下通过 RFB 键入密码并完成登录；密码不出现在日志/磁盘。
2. RFB 连接只监听 loopback，或经我们的加密隧道；不向 LAN 暴露 5900。
3. 切换用户/快速用户切换时不串会话。
4. 失败时降级为 P1 只读，并给出原因。

#### P3 — 长期无人值守（entitlement + 分发）

- 申请 `com.apple.developer.persistent-content-capture`（表单见参考）。
- 使用 Developer ID Application + Developer ID Installer 签 `.pkg`，公证（notarization），hardened runtime。
- 验证：连续 30 天 + 重启 3 次，登录窗口不再出现「继续允许屏幕录制」之类的周期确认。

#### P4 — 企业增强（MDM）

- 受管设备下发 PPPC：预授 Accessibility/PostEvent；下发自动登录/屏幕共享/节能配置。
- 明确告知：**PPPC 不能授予 Screen Recording**，仍需 entitlement 或人工授权。

### 3.3 验收总表

| 能力 | 14.0–14.3 | 14.4+ | 需要 TCC | 需要 entitlement | 备注 |
|---|---|---|---|---|---|
| 锁屏采集 | 可 | 可 | Screen Recording | 否（但每月确认会影响长期） | 用户会话内 |
| 锁屏注入 | 可 | 可 | Accessibility | 否 | 最可靠 |
| 登录窗口采集 | 仅 CGDisplayStream（deprecated，不稳） | SCK 可 | Screen Recording | 长期无人值守需要 | LoginWindow agent |
| 登录窗口注入 | 不保证 | 不保证 | Accessibility/PostEvent | 不解决 | **不承诺** |
| 登录窗口输入（RFB） | 未验证 | 未验证 | 无（Apple 服务） | 否 | 需启用屏幕共享 |
| FileVault 预启动解锁 | 不可行 | 不可行 | — | — | 平台外 |

---

## 4. 风险与限制

### 4.1 必须向用户声明的平台限制（可直接放进产品文档）

1. macOS **不允许**第三方应用在登录窗口可靠地模拟键盘/鼠标；rust-tunnel 在登录窗口默认**只读**。请在机器上登录，或启用「登录窗口输入（实验）」。
2. FileVault 开启时，**重启后的第一次解锁必须在机器前完成**；这是 macOS 的安全设计，任何第三方软件都无法替代。
3. 首次安装需要在机器前授予「屏幕录制」「辅助功能」权限；无人值守场景还需 Apple 的「远程桌面」权限（`persistent-content-capture`）。没有它，系统会周期性要求重新确认。
4. 显示器睡眠会导致画面停止；请关闭睡眠或使用 `pmset`。
5. macOS 14.0–14.3 不支持登录窗口采集；请升级到 14.4+。

### 4.2 安全设计（对照 Windows 的 DACL 加固）

| 风险 | macOS 对策 |
|---|---|
| 普通用户替换 agent 二进制 → 下次登录窗口以 root 执行 | 二进制装到 **root:wheel、目录 755** 的位置（推荐 `/Library/PrivilegedHelperTools/` 或 `/Library/Application Support/<vendor>/`），**不要**让 LaunchAgent 指向 `/Applications`（该目录 admin 组可写）；plist 也 root:wheel 644 |
| 降级/篡改已安装二进制 | 启动前与更新前做 `SecStaticCodeCheckValidity` + Team ID/designated requirement 校验；校验失败拒绝启动并告警 |
| IPC 被本机其他进程冒用 | Unix socket 目录 0700；peer uid 校验 + 对端代码签名校验；不使用 TCP 端口做本机 IPC |
| 令牌/配置泄露 | `host.toml` 与 token 放 root-only（0600）目录；Aqua agent 通过 daemon 获取短期凭据，不直接读全局 token |
| 日志泄露密码 | 登录窗口/锁屏输入路径禁止记录键值；对 `Secure Event Input` 字段只记录布尔 |
| 提权面过大 | daemon 不加载 GUI 框架、不执行 shell、不继承用户环境；所有子进程用绝对路径 |
| 更新竞态 | 先写 `.new` → 校验签名 → 原子 rename；GUI/服务两端版本一致性检查（参考 Windows 的双端更新约束） |
| 后台项被用户关闭 | macOS 13+ 的「登录项与扩展 / Allow in Background」可禁用 agent（Jump Desktop Connect 也要求用户允许后台运行）；启动时检测 agent 是否被禁用并提示，不能静默失败 |
| TCC 反复失效 | 固定 Team ID + bundle id + Developer ID 签名；不要用 ad-hoc 签名做发布 |

> 与 Windows 的差异：macOS 没有 DACL，但有 Unix 权限 + SIP + 代码签名；`/Applications` 对 admin 组可写，所以**不能把 root 运行的二进制放在那里**。任何 admin 用户本来就能 sudo，威胁模型主要是**非管理员本地用户/恶意软件**和**降级攻击**。

### 4.3 合规与分发

- **Mac App Store 不可行**：Guideline 2.4.5 要求沙盒、禁止第三方安装器、禁止请求 root、禁止在共享位置安装代码、禁止未经同意自动启动（A）。Screens 能在商店里，是因为它把采集/注入完全交给 Apple 的屏幕共享服务（B）。
- 分发：Developer ID + 公证（A）；`.pkg` 安装 daemon/agent；提供卸载脚本（`launchctl bootout` + 删文件 + `tccutil reset` 提示）。
- 隐私：采集的是整屏，必须在首次运行与每次会话给出明确提示；登录窗口采集必须在 UI 上可见（不能静默）。
- entitlement 申请：向 Apple 说明「VNC/远程桌面用途」；审核不确定，不能把它当作唯一交付前提（需要 P1 只读兜底）。

---

## 5. 参考链接

**Apple 官方文档 / DTS**

1. Apple Developer Forums — Capturing screen buffer at macOS Login Window with ScreenCaptureKit and PrivilegedHelper（DTS：daemon/agent 分工、`LimitLoadToSessionType`、SCK 14.4+、r.121253782）  
   https://developer.apple.com/forums/thread/814152
2. Apple Developer Forums — loading SMAppService LoginWindow agent（DTS：SMAppService 不支持 LoginWindow）  
   https://developer.apple.com/forums/thread/726864
3. Apple Developer Forums — How to handle TCC permissions on macOS（DTS：系统 TCC.db 受 SIP 保护、designated requirement）  
   https://developer.apple.com/forums/thread/730043
4. Apple Developer Forums — Controlling "Screen Capture" using MDM（Screen Recording 只能 deny）  
   https://developer.apple.com/forums/thread/122414
5. Apple Developer Forums — LaunchAgent without a GUI session?（DTS：会话类型、LoginWindow agent 以 root 运行）  
   https://developer.apple.com/forums/thread/696859
6. Apple Developer Forums — Global Launch Agent loads as root in Login Window Screen（LoginWindow/Aqua 双实例）  
   https://developer.apple.com/forums/thread/674132
7. Apple Developer Forums — Keyboard and Mouse simulation are not working in macOS pre-login window（2016）  
   https://developer.apple.com/forums/thread/62687
8. Apple Developer Forums — CGStreamDisplay and CGEvents for keyboard not working during LoginWindow in intel Mac（2023）  
   https://developer.apple.com/forums/thread/724740
9. Technical Note TN2083: Daemons and Agents  
   https://developer.apple.com/library/archive/technotes/tn2083/_index.html
10. Apple 归档文档 — Customizing Login and Logout（LoginHook 已废弃）  
    https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CustomLogin.html
11. Persistent Content Capture entitlement（申请与用途）  
    https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.persistent-content-capture
12. Persistent Content Capture Entitlement Request（申请表）  
    https://developer.apple.com/contact/request/persistent-content-capture/
13. PrivacyPreferencesPolicyControl（PPPC 载荷）  
    https://developer.apple.com/documentation/devicemanagement/privacypreferencespolicycontrol
14. Apple Platform Deployment — Privacy Preferences Policy Control payload settings（supervised；Screen recording 只能 deny；Accessibility/Post Event 可授予）  
    https://support.apple.com/guide/deployment/dep38df53c2a/web
15. Apple Support — How to log in automatically to a Mac user account（FileVault 禁用自动登录）  
    https://support.apple.com/en-us/102316
16. Apple Support — Control access to screen and system audio recording on Mac  
    https://support.apple.com/guide/mac-help/mchld6aa7d23/mac
17. App Review Guidelines（2.4.5）  
    https://developer.apple.com/app-store/review/guidelines/
18. Notarizing macOS software before distribution  
    https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution
19. ScreenCaptureKit  
    https://developer.apple.com/documentation/screencapturekit
20. CGDisplayStream（deprecated）  
    https://developer.apple.com/documentation/coregraphics/cgdisplaystream
21. CGWindowListCreateImage（deprecated）  
    https://developer.apple.com/documentation/coregraphics/cgwindowlistcreateimage
22. CGEvent.postToPid(_:)  
    https://developer.apple.com/documentation/coregraphics/cgevent/posttopid(_:)
23. Apple Support — Encrypt network data in Remote Desktop（DH + 128-bit AES）  
    https://support.apple.com/guide/remote-desktop/apdfe8e386b/mac
24. Apple Support — Allow a remote computer to access your Mac（屏幕共享/远程管理）  
    https://support.apple.com/guide/mac-help/mchlp1066/mac

**man pages / 工具**

25. launchctl(1)（`asuser`、LoginWindow agent 以 root 运行）  
    https://www.manpagez.com/man/1/launchctl/ （镜像：https://keith.github.io/xcode-man-pages/launchctl.1.html）
26. launchd.plist(5)（`LimitLoadToSessionType`）  
    https://www.manpagez.com/man/5/launchd.plist/
27. tccutil（只能 reset；含 `RemoteDesktop` 服务名）  
    https://ss64.com/mac/tccutil.html
28. pmset（禁用睡眠/显示器睡眠）  
    https://ss64.com/mac/pmset.html

**厂商文档**

29. AnyDesk — Set up Unattended Access（官方称可连接 locked/logged-out devices，含 macOS）  
    https://support.anydesk.com/docs/unattended-access
30. Jump Desktop — macOS Sequoia Screen Recording Policies and Jump Desktop Connect（已持有 persistent-content-capture，显示为「远程桌面」权限）  
    https://support.jumpdesktop.com/hc/en-us/articles/29070118000781-macOS-Sequoia-Screen-Recording-Policies-and-Jump-Desktop-Connect
31. Jump Desktop — Passwordless Logins（「Only When No One Is Logged In」）  
    https://support.jumpdesktop.com/hc/en-us/articles/33278979909389-Passwordless-Logins
32. Jump Desktop — Mac: Jump Desktop Connect does not run after reboot / 后台项权限  
    https://support.jumpdesktop.com/hc/en-us/articles/14343257696269-Mac-Jump-Desktop-Connect-does-not-run-after-reboot-stuck-on-Initializing
33. Screens (Edovia) — Mac Login Window Appears After Connecting（依赖 Remote Management/Screen Sharing 服务）  
    https://support.edovia.com/en/screens-5/troubleshooting/log-in-twice-mac

**第三方实测 / 社区**

34. aetherdesk — M0: macOS spikes — capture and input at lock screen and login window（root daemon/agent、SCK、注入回归、屏幕录制提示）  
    https://github.com/auroval/remote/issues/3
35. aetherdesk — Distribution: Developer ID + notarized pkg（App Store 限制、entitlement、PPPC 限制）  
    https://github.com/auroval/remote/issues/28
36. aetherdesk — Spike: log in at the login window through macOS Screen Sharing（RFB type 30 成功）  
    https://github.com/auroval/remote/issues/72
37. RustDesk Discussion #7762 — How to make macOS RustDesk client starts at boot (on login screen)?（RustDesk 不支持登录窗口；AnyDesk 对照）  
    https://github.com/rustdesk/rustdesk/discussions/7762
38. TeamViewer Community — And again the login window on macOS stucks…（登录窗口不稳定；自带屏幕共享兜底）  
    https://community.teamviewer.com/English/discussion/69116/and-again-the-login-window-on-macos-stucks/p1
39. Michael Tsai — Sequoia Screen Recording Prompts and the Persistent Content Capture Entitlement  
    https://mjtsai.com/blog/2024/08/08/sequoia-screen-recording-prompts-and-the-persistent-content-capture-entitlement/
40. Macworld — How to manage FileVault to maintain remote access to your Mac（`fdesetup authrestart` 的边界）  
    https://www.macworld.com/article/2568036/how-to-manage-filevault-to-maintain-remote-access-to-your-mac.html
41. Zenn — Three Assumptions I Made When Connecting to macOS Screen Sharing with a Custom RFB Client（ARD type 30 / type 2）  
    https://zenn.dev/musicflow/articles/macos-screen-sharing-rfb-traps?locale=en
42. unix-ninja — Enabling macOS screen sharing (VNC) via command line（`kickstart` 启用屏幕共享）  
    https://www.unix-ninja.com/p/Enabling_macOS_screen_sharing_VNC_via_command_line/
