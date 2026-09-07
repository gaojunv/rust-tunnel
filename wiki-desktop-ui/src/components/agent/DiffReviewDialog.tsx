/**
 * DiffReviewDialog —— turn 级聚合 diff 全屏审查弹层（2A'）
 *
 * 数据源：codec.ts TurnDiffView（view.turnDiffs 中某 turn 的完整快照）。
 * 每文件卡片：`fsReadFileText(绝对路径)` 读磁盘当前内容作「变更后」，
 * 内存中对其应用**反向** patch 还原「变更前」，before/after 喂既有 DiffView；
 * 反向失败的文件回退为原始 diff 文本预览（pre）并标注「无法精确还原」。
 *
 * 拒绝流程 = 两个独立动作（文案显式区分）：
 *  1) 「撤销文件改动」：勾选文件逐个 fsReadFileText → 反向应用 → fsWriteFile；
 *     失败的列出并提供「复制反向补丁」手动还原。
 *  2) 「回滚对话历史」：拒绝成功后二次确认 → threadRevert（只动对话，不动文件）。
 *
 * 受控组件：打开/关闭与后续状态由父级（2E）持有；本组件不 import 全局状态。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, Check, Copy, FileDiff, Undo2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { DiffView } from "@/components/DiffView";
import {
  parseAggregatedDiff,
  resolveDiffPath,
  reverseApplyToCurrent,
  reverseFilePatch,
  toDisplayPath,
} from "@/lib/agent/diff-review";
import type { FilePatch } from "@/lib/agent/diff-review";
import {
  encodeUtf8ToBase64,
  fsReadFileText,
  fsWriteFile,
  threadRevert,
} from "@/lib/agent/client";

export type DiffReviewDialogProps = {
  /** 是否打开（父级受控） */
  open: boolean;
  /** 当前审查的 turn（2E 传入 TurnDiffView.turnId） */
  turnId: string | null;
  /** 该 turn 的聚合 unified diff（codec TurnDiffView.diff） */
  diff: string | null;
  /** 会话 id（二次确认「回滚对话历史」时 threadRevert 用） */
  threadId: string | null;
  /** vault 根目录（绝对路径。diff 相对路径据此解析；为空时仅展示、禁用写回） */
  vaultRoot: string | null;
  /** 关闭弹层 */
  onClose: () => void;
  /** 单文件撤销成功后回调（2E 用以刷新 vault/清除审查态） */
  onFilesReverted?: (paths: string[]) => void;
  /** 对话历史回滚成功后回调（2E 用以刷新消息流） */
  onThreadReverted?: (threadId: string) => void;
};

// —— 卡片加载态 ——

type FileCardState =
  | { status: "loading" }
  | {
      status: "preview"; // before/after 精确还原成功，走 DiffView
      absPath: string;
      before: string;
      after: string;
    }
  | {
      status: "fallback"; // 反向失败：原始 diff 文本预览 + 标注
      absPath: string;
      after: string;
      reason: string;
      reverseText: string | null;
    }
  | { status: "error"; absPath: string; message: string };

function copyToClipboard(text: string): Promise<void> {
  // navigator.clipboard 在非安全上下文不可用时回退 execCommand
  if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
    return navigator.clipboard.writeText(text);
  }
  return new Promise((resolve, reject) => {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      if (ok) resolve();
      else reject(new Error("复制失败"));
    } catch (e) {
      reject(e instanceof Error ? e : new Error(String(e)));
    }
  });
}

// —— 单文件卡片 ——

function FileCard({
  filePatch,
  checked,
  onToggle,
  state,
  copied,
  onCopyReverse,
}: {
  filePatch: FilePatch;
  checked: boolean;
  onToggle: () => void;
  state: FileCardState;
  copied: boolean;
  onCopyReverse: () => void;
}) {
  const display = filePatch.rawPath || filePatch.path || "(未知文件)";
  const kindLabel =
    filePatch.kind === "create" ? "新增" : filePatch.kind === "delete" ? "删除" : "修改";

  return (
    <div className="overflow-hidden rounded-md border">
      <div className="flex items-center gap-2 border-b bg-muted/30 px-2 py-1.5 text-xs">
        <input
          type="checkbox"
          checked={checked}
          onChange={onToggle}
          aria-label={`选择${display}以撤销改动`}
          className="h-3.5 w-3.5 shrink-0"
        />
        <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={display}>
          {display}
        </span>
        <span className="shrink-0 rounded bg-muted px-1 py-0.5 text-[10px] text-muted-foreground">
          {kindLabel}
        </span>
        <span className="shrink-0 font-mono text-[10px]">
          <span className="text-emerald-600 dark:text-emerald-400">+{filePatch.adds}</span>
          <span className="mx-0.5 text-muted-foreground">·</span>
          <span className="text-red-600 dark:text-red-400">-{filePatch.dels}</span>
        </span>
      </div>
      <div className="p-2">
        {state.status === "loading" && (
          <p className="px-1 py-3 text-center text-[11px] text-muted-foreground">
            正在读取磁盘当前内容…
          </p>
        )}
        {state.status === "error" && (
          <div className="rounded border border-red-500/25 bg-red-500/10 px-2.5 py-2 text-[11px]">
            <p className="font-medium text-red-600 dark:text-red-300">无法读取文件</p>
            <p className="mt-0.5 break-words font-mono text-[11px] text-muted-foreground">
              {state.absPath || display}：{state.message}
            </p>
            <details className="mt-1.5">
              <summary className="cursor-pointer text-muted-foreground">查看原始 diff</summary>
              <ScrollArea className="mt-1 max-h-64 overflow-hidden rounded bg-muted">
                <pre className="whitespace-pre-wrap break-words px-2.5 py-2 font-mono text-[11px]">
                  {filePatch.patch.slice(0, 16000)}
                </pre>
              </ScrollArea>
            </details>
          </div>
        )}
        {state.status === "preview" && (
          <DiffView
            localText={state.before}
            remoteText={state.after}
            localLabel="变更前（由当前内容反推）"
            remoteLabel="变更后（磁盘当前）"
          />
        )}
        {state.status === "fallback" && (
          <div className="space-y-2">
            <div className="flex items-start gap-1.5 rounded border border-amber-500/25 bg-amber-500/10 px-2.5 py-2 text-[11px]">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-600 dark:text-amber-400" />
              <div>
                <p className="font-medium">无法精确还原该文件的变更前内容</p>
                <p className="mt-0.5 text-muted-foreground">{state.reason}。下方为原始 diff 文本预览。</p>
              </div>
            </div>
            <ScrollArea className="max-h-80 overflow-hidden rounded bg-muted">
              <pre className="whitespace-pre-wrap break-words px-2.5 py-2 font-mono text-[11px]">
                {filePatch.patch.slice(0, 16000)}
              </pre>
            </ScrollArea>
            {state.reverseText && (
              <div className="flex justify-end">
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  className="h-6 gap-1 text-[11px] text-muted-foreground hover:text-foreground"
                  onClick={onCopyReverse}
                >
                  {copied ? (
                    <>
                      <Check className="h-3 w-3" /> 已复制
                    </>
                  ) : (
                    <>
                      <Copy className="h-3 w-3" /> 复制反向补丁（手动还原用）
                    </>
                  )}
                </Button>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

// —— 主弹层 ——

export function DiffReviewDialog({
  open,
  turnId,
  diff,
  threadId,
  vaultRoot,
  onClose,
  onFilesReverted,
  onThreadReverted,
}: DiffReviewDialogProps) {
  // 切分聚合 diff（坏块在 parse 内跳过；解析后无可用块 → 整体兜底展示原文）
  const patches = useMemo<FilePatch[]>(() => {
    if (!diff) return [];
    try {
      return parseAggregatedDiff(diff);
    } catch {
      return [];
    }
  }, [diff]);

  const [checked, setChecked] = useState<Record<string, boolean>>({});
  const [states, setStates] = useState<Record<string, FileCardState>>({});
  const [working, setWorking] = useState(false);
  const [results, setResults] = useState<
    Array<{ path: string; ok: boolean; message?: string; reverseText?: string | null }>
  >([]);
  const [sandboxError, setSandboxError] = useState<string | null>(null);
  const [askRevertThread, setAskRevertThread] = useState(false);
  const [revertingThread, setRevertingThread] = useState(false);
  const [threadReverted, setThreadReverted] = useState(false);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  // 打开时重置审查态
  useEffect(() => {
    if (!open) return;
    setResults([]);
    setSandboxError(null);
    setAskRevertThread(false);
    setThreadReverted(false);
    setCopiedKey(null);
    setChecked(Object.fromEntries(patches.map((p, i) => [patchKey(p, i), true])));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, turnId]);

  // 每文件读磁盘当前内容 → 反向还原 before（vaultRoot 缺失时直接 fallback）
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    const next: Record<string, FileCardState> = {};
    for (let i = 0; i < patches.length; i++) {
      next[patchKey(patches[i], i)] = { status: "loading" };
    }
    setStates(next);

    (async () => {
      for (let i = 0; i < patches.length; i++) {
        const key = patchKey(patches[i], i);
        if (!vaultRoot) {
          if (!cancelled) {
            setStates((s) => ({
              ...s,
              [key]: {
                status: "fallback",
                absPath: patches[i].path,
                after: "",
                reason: "未知 vault 根目录，无法读取磁盘内容",
                reverseText: reverseFilePatch(patches[i]),
              },
            }));
          }
          continue;
        }
        // 无有效 path（如纯兜底块）→ 直接原文展示
        if (!patches[i].structured || !patches[i].path) {
          if (!cancelled) {
            setStates((s) => ({
              ...s,
              [key]: {
                status: "fallback",
                absPath: patches[i].path,
                after: "",
                reason: "该补丁无法结构化解析（非标准 unified diff）",
                reverseText: null,
              },
            }));
          }
          continue;
        }
        const abs = resolveDiffPath(patches[i].path, vaultRoot);
        try {
          const after = await fsReadFileText(abs);
          const back = reverseApplyToCurrent(after, patches[i]);
          if (cancelled) return;
          if ("content" in back) {
            setStates((s) => ({ ...s, [key]: { status: "preview", absPath: abs, before: back.content, after } }));
          } else {
            setStates((s) => ({
              ...s,
              [key]: {
                status: "fallback",
                absPath: abs,
                after,
                reason: back.reason,
                reverseText: reverseFilePatch(patches[i]),
              },
            }));
          }
        } catch (e) {
          if (cancelled) return;
          const message = e instanceof Error ? e.message : String(e);
          setStates((s) => ({ ...s, [key]: { status: "error", absPath: abs, message } }));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, turnId, vaultRoot]);

  const selectedKeys = useMemo(
    () => patches.map((p, i) => patchKey(p, i)).filter((k) => checked[k]),
    [patches, checked],
  );

  const toggleOne = useCallback((key: string) => {
    setChecked((c) => ({ ...c, [key]: !c[key] }));
  }, []);

  const toggleAll = useCallback(
    (on: boolean) => {
      setChecked(Object.fromEntries(patches.map((p, i) => [patchKey(p, i), on])));
    },
    [patches],
  );

  // 「拒绝选中文件（撤销磁盘改动）」：逐个 try/catch，失败逐项列出
  const handleRejectSelected = useCallback(async () => {
    if (working || selectedKeys.length === 0) return;
    setWorking(true);
    setResults([]);
    setSandboxError(null);
    setAskRevertThread(false);
    const out: Array<{ path: string; ok: boolean; message?: string; reverseText?: string | null }> = [];
    const okPaths: string[] = [];
    for (const key of selectedKeys) {
      const idx = patches.findIndex((p, i) => patchKey(p, i) === key);
      const p = patches[idx];
      if (!p) continue;
      if (!vaultRoot) {
        out.push({ path: p.rawPath || "(未知)", ok: false, message: "未知 vault 根，无法定位文件", reverseText: reverseFilePatch(p) });
        continue;
      }
      if (!p.structured || !p.path) {
        out.push({ path: p.rawPath || "(未知)", ok: false, message: "补丁不可结构化解析，无法自动撤销", reverseText: null });
        continue;
      }
      const abs = resolveDiffPath(p.path, vaultRoot);
      try {
        // 重新读盘（防 stale）：fsReadFileText → 反向应用 → fsWriteFile
        const after = await fsReadFileText(abs);
        const back = reverseApplyToCurrent(after, p);
        if (!("content" in back)) {
          out.push({ path: toDisplayPath(abs, vaultRoot), ok: false, message: back.reason, reverseText: reverseFilePatch(p) });
          continue;
        }
        await fsWriteFile({ path: abs, dataBase64: encodeUtf8ToBase64(back.content) });
        okPaths.push(abs);
        out.push({ path: toDisplayPath(abs, vaultRoot), ok: true });
      } catch (e) {
        const message =
          e instanceof Error ? e.message : String(e);
        // app-server 沙箱/写权限失败 → 整体降级提示 + 单项反向补丁备用
        out.push({
          path: toDisplayPath(abs, vaultRoot),
          ok: false,
          message: `写入失败（可能受 app-server 沙箱限制）：${message}`,
          reverseText: reverseFilePatch(p),
        });
      }
    }
    setResults(out);
    setWorking(false);
    if (okPaths.length > 0) {
      onFilesReverted?.(okPaths);
      // 至少一个成功 → 显式二次确认「同时回滚对话历史？」
      // 注意：thread/revert 只回滚对话，不再动文件
      setAskRevertThread(true);
    } else if (out.some((r) => !r.ok && (r.message ?? "").includes("沙箱"))) {
      setSandboxError("文件写回失败，可能受 app-server 沙箱限制。可复制各文件的「反向补丁」手动还原。");
    }
  }, [working, selectedKeys, patches, vaultRoot, onFilesReverted]);

  // 二次确认：回滚对话历史（thread/revert 只动对话，不动文件）
  const handleRevertThread = useCallback(async () => {
    if (!threadId || !turnId) return;
    setRevertingThread(true);
    try {
      await threadRevert({ threadId, beforeTurnId: turnId });
      setThreadReverted(true);
      onThreadReverted?.(threadId);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setResults((r) => [...r, { path: "(对话历史)", ok: false, message: `回滚对话失败：${message}` }]);
    } finally {
      setRevertingThread(false);
    }
  }, [threadId, turnId, onThreadReverted]);

  const handleCopyReverse = useCallback(
    async (key: string, reverseText: string | null) => {
      if (!reverseText) return;
      try {
        await copyToClipboard(reverseText);
        setCopiedKey(key);
      } catch {
        setCopiedKey(null);
      }
    },
    [],
  );

  if (!open) return null;

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="flex max-h-[90vh] w-[min(96vw,860px)] flex-col rounded-lg border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="审查本回合文件变更"
      >
        <div className="flex items-center justify-between border-b px-4 py-3">
          <h2 className="flex items-center gap-2 text-sm font-semibold">
            <FileDiff className="h-4 w-4" />
            审查本回合文件变更
            {patches.length > 0 && (
              <span className="font-normal text-muted-foreground">（{patches.length} 文件）</span>
            )}
          </h2>
          <Button type="button" size="sm" variant="ghost" className="h-7 w-7 p-0" onClick={onClose} aria-label="关闭审查">
            <X className="h-4 w-4" />
          </Button>
        </div>

        <div className="min-h-0 flex-1 overflow-auto p-4">
          {patches.length === 0 ? (
            // 不可解析整体 → 原文兜底展示
            <div className="space-y-2">
              <p className="text-xs text-muted-foreground">
                该回合 diff 无法按文件切分（可能为非标准格式），下方展示原始文本。
              </p>
              <ScrollArea className="max-h-[60vh] overflow-hidden rounded bg-muted">
                <pre className="whitespace-pre-wrap break-words px-3 py-2 font-mono text-[11px]">
                  {(diff ?? "").slice(0, 32000)}
                </pre>
              </ScrollArea>
            </div>
          ) : (
            <div className="space-y-3">
              <div className="flex items-center justify-between text-[11px] text-muted-foreground">
                <span>已选 {selectedKeys.length}/{patches.length} 文件</span>
                <span className="flex gap-1">
                  <Button type="button" size="sm" variant="ghost" className="h-6 text-[11px]" onClick={() => toggleAll(true)}>
                    全选
                  </Button>
                  <Button type="button" size="sm" variant="ghost" className="h-6 text-[11px]" onClick={() => toggleAll(false)}>
                    全不选
                  </Button>
                </span>
              </div>
              {patches.map((p, i) => {
                const key = patchKey(p, i);
                const st = states[key] ?? { status: "loading" as const };
                const rev = st.status === "fallback" ? st.reverseText : null;
                return (
                  <FileCard
                    key={key}
                    filePatch={p}
                    checked={!!checked[key]}
                    onToggle={() => toggleOne(key)}
                    state={st}
                    copied={copiedKey === key}
                    onCopyReverse={() => handleCopyReverse(key, rev)}
                  />
                );
              })}
            </div>
          )}

          {sandboxError && (
            <div className="mt-3 flex items-start gap-1.5 rounded border border-amber-500/25 bg-amber-500/10 px-2.5 py-2 text-[11px]">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-600 dark:text-amber-400" />
              <p>{sandboxError}</p>
            </div>
          )}

          {results.length > 0 && (
            <div className="mt-3 rounded border p-2.5 text-[11px]">
              <p className="font-medium">
                撤销结果：成功 {results.filter((r) => r.ok).length}/{results.length}
              </p>
              <ul className="mt-1.5 space-y-1.5">
                {results.map((r, idx) => (
                  <li
                    key={idx}
                    className={`flex items-start justify-between gap-2 rounded px-2 py-1 ${
                      r.ok ? "bg-emerald-500/10" : "bg-red-500/10"
                    }`}
                  >
                    <div className="min-w-0">
                      <span className="font-mono">{r.path}</span>
                      <span className="ml-1.5">{r.ok ? "已还原" : `失败：${r.message ?? "未知错误"}`}</span>
                    </div>
                    {!r.ok && r.reverseText && (
                      <Button
                        type="button"
                        size="sm"
                        variant="ghost"
                        className="h-6 shrink-0 gap-1 text-[11px]"
                        onClick={() => handleCopyReverse(`result-${idx}`, r.reverseText ?? null)}
                      >
                        {copiedKey === `result-${idx}` ? (
                          <>
                            <Check className="h-3 w-3" /> 已复制
                          </>
                        ) : (
                          <>
                            <Copy className="h-3 w-3" /> 复制反向补丁
                          </>
                        )}
                      </Button>
                    )}
                  </li>
                ))}
              </ul>
            </div>
          )}

          {askRevertThread && !threadReverted && (
            <div className="mt-3 rounded border border-blue-500/25 bg-blue-500/10 px-3 py-2.5 text-xs">
              <p className="font-medium">文件改动已撤销。是否同时回滚该回合的对话历史？</p>
              <p className="mt-1 text-muted-foreground">
                说明：「撤销文件改动」只还原磁盘文件；「回滚对话历史」只删除该回合及之后的对话消息，
                <span className="font-medium text-foreground">不会再改动文件</span>。两者是独立动作。
              </p>
              <div className="mt-2 flex gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  disabled={revertingThread || !threadId}
                  onClick={handleRevertThread}
                >
                  {revertingThread ? "回滚中…" : "是，回滚对话历史"}
                </Button>
                <Button type="button" size="sm" variant="ghost" onClick={() => setAskRevertThread(false)}>
                  否，仅保留对话
                </Button>
              </div>
              {!threadId && (
                <p className="mt-1 text-[11px] text-muted-foreground">（缺少 threadId，无法回滚对话）</p>
              )}
            </div>
          )}
          {threadReverted && (
            <div className="mt-3 rounded border border-emerald-500/25 bg-emerald-500/10 px-3 py-2 text-xs">
              对话历史已回滚（文件不再变动）。
            </div>
          )}
        </div>

        <div className="flex flex-wrap items-center justify-between gap-2 border-t px-4 py-3">
          <p className="max-w-[60%] text-[11px] text-muted-foreground">
            拒绝 = 仅撤销所选文件的磁盘改动；接受 = 文件保留现状并关闭。回滚对话是单独的二次确认。
          </p>
          <div className="flex gap-2">
            <Button type="button" size="sm" variant="outline" onClick={onClose}>
              关闭
            </Button>
            <Button
              type="button"
              size="sm"
              variant="destructive"
              className="gap-1"
              disabled={working || selectedKeys.length === 0 || !vaultRoot}
              onClick={handleRejectSelected}
            >
              <Undo2 className="h-3.5 w-3.5" />
              {working ? "撤销中…" : `拒绝选中文件（撤销磁盘改动，${selectedKeys.length}）`}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}

function patchKey(p: FilePatch, index: number): string {
  return `${p.path || p.rawPath || "unknown"}#${index}`;
}
