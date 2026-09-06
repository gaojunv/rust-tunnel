/**
 * 审批弹窗 —— 全局队列一次显示队首，按 method 分派渲染
 * 依赖 allowlist 模块级记忆，命中自动批准不再弹窗
 */
import { useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { DiffView } from "@/components/DiffView";
import type { ApprovalRequestView, AgentThreadView } from "@/lib/agent/codec";
import type { Allowlist } from "@/lib/agent/allowlist";
import { allowlistKeys, allowlistRemove, allowlistClear } from "@/lib/agent/allowlist";

type Props = {
  queue: ApprovalRequestView[];
  view?: AgentThreadView | null;
  onRespond: (id: unknown, result: unknown, error?: unknown) => void;
  allowlist: Allowlist;
  onAllowlistChanged: () => void;
};

function renderCommandParams(params: unknown): {
  command: string;
  cwd: string;
  reason: string;
} {
  const p = (params ?? {}) as Record<string, unknown>;
  const command =
    typeof p["command"] === "string"
      ? (p["command"] as string)
      : Array.isArray(p["command"])
        ? (p["command"] as string[]).join(" ")
        : "";
  const cwd =
    typeof p["cwd"] === "string"
      ? (p["cwd"] as string)
      : p["cwd"] != null
        ? String(p["cwd"])
        : "";
  const reason = typeof p["reason"] === "string" ? (p["reason"] as string) : "";
  return { command, cwd, reason };
}

function renderFileChangeParams(params: unknown, view?: AgentThreadView | null): {
  reason: string;
  grantRoot: string;
  itemId: string;
  filesFromView: Array<{ path: string; kind: string; diff: string }>;
} {
  const p = (params ?? {}) as Record<string, unknown>;
  const reason = typeof p["reason"] === "string" ? (p["reason"] as string) : "";
  const grantRoot = typeof p["grantRoot"] === "string" ? (p["grantRoot"] as string) : "";
  const itemId = typeof p["itemId"] === "string" ? (p["itemId"] as string) : "";
  let filesFromView: Array<{ path: string; kind: string; diff: string }> = [];
  if (itemId && view) {
    const f = view.items.find((it) => it.id === itemId && it.kind === "fileChange");
    if (f && f.kind === "fileChange") {
      filesFromView = f.files;
    }
  }
  return { reason, grantRoot, itemId, filesFromView };
}

function renderPermissionsParams(params: unknown): {
  reason: string;
  cwd: string;
  permissions: unknown;
} {
  const p = (params ?? {}) as Record<string, unknown>;
  const reason = typeof p["reason"] === "string" ? (p["reason"] as string) : "";
  const cwd =
    typeof p["cwd"] === "string"
      ? (p["cwd"] as string)
      : p["cwd"] != null
        ? String(p["cwd"])
        : "";
  const permissions = p["permissions"] ?? null;
  return { reason, cwd, permissions };
}

function renderLegacyExec(params: unknown) {
  const p = (params ?? {}) as Record<string, unknown>;
  const command = Array.isArray(p["command"]) ? (p["command"] as string[]).join(" ") : "";
  const cwd = typeof p["cwd"] === "string" ? (p["cwd"] as string) : "";
  const reason = typeof p["reason"] === "string" ? (p["reason"] as string) : "";
  return { command, cwd, reason };
}

function renderLegacyPatch(params: unknown) {
  const p = (params ?? {}) as Record<string, unknown>;
  const reason = typeof p["reason"] === "string" ? (p["reason"] as string) : "";
  const grantRoot = typeof p["grantRoot"] === "string" ? (p["grantRoot"] as string) : "";
  const fileChanges = (p["fileChanges"] ?? {}) as Record<string, unknown>;
  const entries = Object.entries(fileChanges).map(([path, v]) => ({
    path,
    raw: v,
  }));
  return { reason, grantRoot, entries };
}

export function ApprovalDialog({ queue, view, onRespond, allowlist, onAllowlistChanged }: Props) {
  const [showAllowlist, setShowAllowlist] = useState(false);
  const head = queue[0];
  if (!head) return null;

  const handleApproveOnce = () => {
    const { method, id, params } = head;
    switch (method) {
      case "item/commandExecution/requestApproval":
        onRespond(id, { decision: "accept" as const });
        break;
      case "item/fileChange/requestApproval":
        onRespond(id, { decision: "accept" as const });
        break;
      case "item/permissions/requestApproval": {
        const p = (params ?? {}) as Record<string, unknown>;
        const requested = (p["permissions"] ?? { network: null, fileSystem: null }) as unknown;
        // 按请求的权限原样授予，scope 为 turn（单次）
        // GrantedPermissionProfile 形状与 RequestPermissionProfile 兼容，字段为可选
        onRespond(id, { permissions: requested as Record<string, unknown>, scope: "turn" as const });
        break;
      }
      case "execCommandApproval":
        onRespond(id, { decision: "approved" as const });
        break;
      case "applyPatchApproval":
        onRespond(id, { decision: "approved" as const });
        break;
      default:
        onRespond(id, { decision: "accept" as const });
        break;
    }
  };

  const handleRememberSession = () => {
    const { method, id, params } = head;
    // 本会话记住：切换为 acceptForSession / approved_for_session / scope session
    switch (method) {
      case "item/commandExecution/requestApproval":
        onRespond(id, { decision: "acceptForSession" as const });
        break;
      case "item/fileChange/requestApproval":
        onRespond(id, { decision: "acceptForSession" as const });
        break;
      case "item/permissions/requestApproval": {
        const p = (params ?? {}) as Record<string, unknown>;
        const requested = (p["permissions"] ?? { network: null, fileSystem: null }) as unknown;
        onRespond(id, { permissions: requested as Record<string, unknown>, scope: "session" as const });
        break;
      }
      case "execCommandApproval":
        onRespond(id, { decision: "approved_for_session" as const });
        break;
      case "applyPatchApproval":
        onRespond(id, { decision: "approved_for_session" as const });
        break;
      default:
        onRespond(id, { decision: "acceptForSession" as const });
        break;
    }
    // 注意：allowlist 的写入由 AgentPanel 的 onServerRequest 自动处理（buildAllowlistKey），
    // 此处仅负责发送 session 级批准，队列侧的 allowlist 由外层在收到 "记住" 点击后调用 allowlistAdd
  };

  const handleDecline = () => {
    const { method, id } = head;
    switch (method) {
      case "item/commandExecution/requestApproval":
        onRespond(id, { decision: "decline" as const });
        break;
      case "item/fileChange/requestApproval":
        onRespond(id, { decision: "decline" as const });
        break;
      case "item/permissions/requestApproval":
        // 权限拒绝通过 error 回传，前端以 error 形式 decline
        onRespond(id, undefined, { code: -1, message: "用户拒绝权限请求" });
        break;
      case "execCommandApproval":
        onRespond(id, { decision: { denied: { rejection: "用户拒绝" } } });
        break;
      case "applyPatchApproval":
        onRespond(id, { decision: { denied: { rejection: "用户拒绝" } } });
        break;
      default:
        onRespond(id, { decision: "decline" as const });
        break;
    }
  };

  let body: React.ReactNode = null;
  const method = head.method;
  if (method === "item/commandExecution/requestApproval") {
    const { command, cwd, reason } = renderCommandParams(head.params);
    body = (
      <div className="space-y-3">
        <p className="text-xs font-medium">命令执行审批</p>
        {reason && <p className="text-xs text-muted-foreground">理由：{reason}</p>}
        {cwd && <p className="text-xs text-muted-foreground">工作目录：{cwd}</p>}
        <div className="rounded bg-muted p-2">
          <p className="text-[11px] text-muted-foreground">命令</p>
          <pre className="mt-1 whitespace-pre-wrap break-words font-mono text-xs">{command || "(空命令)"}</pre>
        </div>
        <p className="text-[11px] text-muted-foreground">批准后将执行上述命令。</p>
      </div>
    );
  } else if (method === "item/fileChange/requestApproval") {
    const { reason, grantRoot, itemId, filesFromView } = renderFileChangeParams(head.params, view);
    body = (
      <div className="space-y-3">
        <p className="text-xs font-medium">文件变更审批</p>
        {reason && <p className="text-xs text-muted-foreground">理由：{reason}</p>}
        {grantRoot && <p className="text-xs text-muted-foreground">授权根：{grantRoot}</p>}
        {itemId && <p className="text-[11px] text-muted-foreground">itemId：{itemId}</p>}
        {filesFromView.length > 0 ? (
          <div className="space-y-2">
            <p className="text-xs font-medium">变更文件（{filesFromView.length}）</p>
            {filesFromView.map((f) => (
              <div key={f.path} className="rounded border p-2">
                <p className="font-mono text-[11px]">{f.path} · {f.kind}</p>
                {f.diff ? (
                  <div className="mt-2">
                    {/* 复用 DiffView：若 diff 为统一 diff，直接按文本对比展示 */}
                    <DiffView localText="" remoteText={f.diff} localLabel="变更前" remoteLabel="变更后" />
                  </div>
                ) : (
                  <p className="mt-1 text-[11px] text-muted-foreground">（无 diff 预览）</p>
                )}
              </div>
            ))}
          </div>
        ) : (
          <div className="rounded border bg-muted/20 p-3 text-xs text-muted-foreground">
            暂无关联 fileChange 项的 diff 预览（可能在 turn/diff/updated 中）。授权后 Agent 将写入 vault。
          </div>
        )}
      </div>
    );
  } else if (method === "item/permissions/requestApproval") {
    const { reason, cwd, permissions } = renderPermissionsParams(head.params);
    body = (
      <div className="space-y-3">
        <p className="text-xs font-medium">权限请求审批</p>
        {reason && <p className="text-xs text-muted-foreground">理由：{reason}</p>}
        {cwd && <p className="text-xs text-muted-foreground">工作目录：{cwd}</p>}
        <div className="rounded bg-muted p-2">
          <p className="text-[11px] text-muted-foreground">请求权限</p>
          <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words text-[11px]">
            {JSON.stringify(permissions, null, 2)}
          </pre>
        </div>
      </div>
    );
  } else if (method === "execCommandApproval") {
    const { command, cwd, reason } = renderLegacyExec(head.params);
    body = (
      <div className="space-y-3">
        <p className="text-xs font-medium">执行命令审批（legacy）</p>
        {reason && <p className="text-xs text-muted-foreground">理由：{reason}</p>}
        {cwd && <p className="text-xs text-muted-foreground">工作目录：{cwd}</p>}
        <pre className="rounded bg-muted p-2 font-mono text-xs">{command || "(空)"}</pre>
      </div>
    );
  } else if (method === "applyPatchApproval") {
    const { reason, grantRoot, entries } = renderLegacyPatch(head.params);
    body = (
      <div className="space-y-3">
        <p className="text-xs font-medium">应用补丁审批（legacy）</p>
        {reason && <p className="text-xs text-muted-foreground">理由：{reason}</p>}
        {grantRoot && <p className="text-xs text-muted-foreground">授权根：{grantRoot}</p>}
        <p className="text-xs">文件数：{entries.length}</p>
        {entries.slice(0, 8).map((e) => (
          <div key={e.path} className="rounded border p-2">
            <p className="font-mono text-[11px]">{e.path}</p>
            <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-words text-[11px]">
              {JSON.stringify(e.raw, null, 2).slice(0, 2000)}
            </pre>
          </div>
        ))}
      </div>
    );
  } else {
    body = (
      <div className="space-y-2">
        <p className="text-xs font-medium">审批请求：{method}</p>
        <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">{JSON.stringify(head.params, null, 2)}</pre>
      </div>
    );
  }

  const allowKeys = allowlistKeys(allowlist);
  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) {
          // 点击遮罩不关闭，需明确操作
        }
      }}
    >
      <div
        className="flex max-h-[85vh] w-[min(96vw,720px)] flex-col rounded-lg border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b px-4 py-3">
          <h2 className="text-sm font-semibold">Agent 审批</h2>
          <span className="text-xs text-muted-foreground">队列 {queue.length} 项 · 当前 1/{queue.length}</span>
        </div>
        <div className="min-h-0 flex-1 overflow-auto p-4">{body}</div>
        <div className="flex flex-wrap items-center justify-between gap-2 border-t px-4 py-3">
          <div className="flex flex-wrap gap-2">
            <Button type="button" size="sm" onClick={handleApproveOnce}>
              批准一次
            </Button>
            <Button type="button" size="sm" variant="secondary" onClick={handleRememberSession}>
              本会话记住
            </Button>
            <Button type="button" size="sm" variant="outline" onClick={handleDecline}>
              拒绝
            </Button>
          </div>
          <div className="flex items-center gap-2">
            {allowKeys.length > 0 && (
              <Button type="button" size="sm" variant="ghost" onClick={() => setShowAllowlist((v) => !v)}>
                已记住 {allowKeys.length}
              </Button>
            )}
          </div>
        </div>
        {showAllowlist && allowKeys.length > 0 && (
          <div className="border-t bg-muted/30 px-4 py-3">
            <div className="flex items-center justify-between">
              <p className="text-xs font-medium">本会话已记住</p>
              <Button
                type="button"
                size="sm"
                variant="ghost"
                className="h-6 text-xs"
                onClick={() => {
                  allowlistClear(allowlist);
                  onAllowlistChanged();
                }}
              >
                清除全部
              </Button>
            </div>
            <ul className="mt-2 space-y-1">
              {allowKeys.map((k) => (
                <li key={k} className="flex items-center justify-between rounded bg-background px-2 py-1">
                  <span className="min-w-0 flex-1 truncate font-mono text-[11px]">{k}</span>
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    className="h-6 text-xs"
                    onClick={() => {
                      allowlistRemove(allowlist, k);
                      onAllowlistChanged();
                    }}
                  >
                    清除
                  </Button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
