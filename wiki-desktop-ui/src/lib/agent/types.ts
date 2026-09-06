/**
 * Codex app-server 协议核心类型 barrel
 * 统一导出 vendor 目录中后续开发会用到的关键类型
 * 上游 pin: rust-v0.153.4（见 src/lib/agent/types/VERSION）
 * 生成方式：bash scripts/vendor-codex-types.sh（CODEX_VERSION 可覆写）
 */

// ── 传输层（JSON-RPC） ──
export type { ClientRequest } from "./types/ClientRequest";
export type { ClientNotification } from "./types/ClientNotification";
export type { ServerRequest } from "./types/ServerRequest";
export type { ServerNotification } from "./types/ServerNotification";
export type { ServerNotificationEnvelope } from "./types/ServerNotificationEnvelope";
export type { RequestId } from "./types/RequestId";

// ── 握手 ──
export type { InitializeParams } from "./types/InitializeParams";
export type { InitializeResponse } from "./types/InitializeResponse";
export type { InitializeCapabilities } from "./types/InitializeCapabilities";
export type { ClientInfo } from "./types/ClientInfo";

// ── 顶层通用 ──
export type { ThreadId } from "./types/ThreadId";

// ── 审批（legacy，未进 v2 的旧路径） ──
export type { ApplyPatchApprovalParams } from "./types/ApplyPatchApprovalParams";
export type { ApplyPatchApprovalResponse } from "./types/ApplyPatchApprovalResponse";
export type { ExecCommandApprovalParams } from "./types/ExecCommandApprovalParams";
export type { ExecCommandApprovalResponse } from "./types/ExecCommandApprovalResponse";

// ── v2：Thread / Turn / Item ──
export type { Thread } from "./types/v2/Thread";
export type { ThreadItem } from "./types/v2/ThreadItem";
export type { Turn } from "./types/v2/Turn";

export type { ThreadStartParams } from "./types/v2/ThreadStartParams";
export type { ThreadStartResponse } from "./types/v2/ThreadStartResponse";
export type { ThreadListParams } from "./types/v2/ThreadListParams";
export type { ThreadListResponse } from "./types/v2/ThreadListResponse";
export type { ThreadResumeParams } from "./types/v2/ThreadResumeParams";
export type { ThreadResumeResponse } from "./types/v2/ThreadResumeResponse";

export type { TurnStartParams } from "./types/v2/TurnStartParams";
export type { TurnStartResponse } from "./types/v2/TurnStartResponse";
export type { TurnInterruptParams } from "./types/v2/TurnInterruptParams";
export type { TurnInterruptResponse } from "./types/v2/TurnInterruptResponse";

// ── v2：审批 ──
export type { CommandExecutionRequestApprovalParams } from "./types/v2/CommandExecutionRequestApprovalParams";
export type { CommandExecutionRequestApprovalResponse } from "./types/v2/CommandExecutionRequestApprovalResponse";
export type { FileChangeRequestApprovalParams } from "./types/v2/FileChangeRequestApprovalParams";
export type { FileChangeRequestApprovalResponse } from "./types/v2/FileChangeRequestApprovalResponse";
export type { PermissionsRequestApprovalParams } from "./types/v2/PermissionsRequestApprovalParams";
export type { PermissionsRequestApprovalResponse } from "./types/v2/PermissionsRequestApprovalResponse";

// ── v2：审批辅助枚举/决策 ──
export type { CommandExecutionApprovalDecision } from "./types/v2/CommandExecutionApprovalDecision";
export type { FileChangeApprovalDecision } from "./types/v2/FileChangeApprovalDecision";
export type { AskForApproval } from "./types/v2/AskForApproval";

// ── v2：命名空间（按需深层导入时直接 `import type * as CodexV2 from "./types/v2/index"`） ──
export type { ThreadStatus } from "./types/v2/ThreadStatus";
export type { TurnStatus } from "./types/v2/TurnStatus";
