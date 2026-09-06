/**
 * MessageStream —— 按 ItemView 类型渲染消息流
 * commandExecution: 折叠卡片 + 输出区 + 退出码徽标
 * fileChange: 文件列表 + DiffView 复用
 * plan: checklist 样式
 * reasoning: 默认收起
 * agentMessage: streamdown 流式 + 插入到笔记
 */
import { useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { DiffView } from "@/components/DiffView";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import type { ItemView } from "@/lib/agent/codec";

type Props = {
  items: ItemView[];
  activeTurnId?: string | null;
  onInsertToNote: (text: string) => void;
};

function ExitBadge({ code }: { code: number | null | undefined }) {
  if (code == null) return null;
  const ok = code === 0;
  return (
    <span
      className={`inline-flex items-center rounded px-1.5 py-0.5 text-[11px] font-medium ${
        ok ? "bg-green-500/15 text-green-700 dark:text-green-300" : "bg-red-500/15 text-red-700 dark:text-red-300"
      }`}
    >
      退出码 {code}
    </span>
  );
}

function renderUserMessage(item: Extract<ItemView, { kind: "userMessage" }>) {
  return (
    <div key={item.id} className="self-end max-w-[85%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground">
      <span className="whitespace-pre-wrap break-words">{item.text}</span>
    </div>
  );
}

function renderAgentMessage(
  item: Extract<ItemView, { kind: "agentMessage" }>,
  activeTurnId: string | null | undefined,
  onInsertToNote: (text: string) => void,
) {
  return (
    <div key={item.id} className="self-start max-w-[92%] rounded-lg bg-muted px-3 py-2 text-sm">
      <div className="prose prose-sm max-w-none dark:prose-invert">
        <Streamdown>{item.text || (activeTurnId ? "…" : "")}</Streamdown>
      </div>
      {item.complete && item.text && (
        <div className="mt-2">
          <Button type="button" variant="outline" size="sm" className="h-7 text-xs" onClick={() => onInsertToNote(item.text)}>
            插入到笔记
          </Button>
        </div>
      )}
    </div>
  );
}

function renderReasoning(item: Extract<ItemView, { kind: "reasoning" }>) {
  return (
    <details key={item.id} className="self-start max-w-[92%] rounded-lg border bg-muted/50 px-3 py-2 text-xs">
      <summary className="cursor-pointer text-muted-foreground">思考过程</summary>
      <pre className="mt-2 whitespace-pre-wrap break-words text-xs">{item.text}</pre>
    </details>
  );
}

function renderCommandExecution(item: Extract<ItemView, { kind: "commandExecution" }>) {
  const isRunning = item.status === "inProgress" || item.status === "running";
  return (
    <details
      key={item.id}
      open={isRunning}
      className="self-start max-w-[92%] rounded-lg border bg-card px-3 py-2 text-xs"
    >
      <summary className="flex cursor-pointer items-center gap-2">
        <span className="font-mono text-xs">{"$ "}{item.command || "(空命令)"}</span>
        <span className="ml-auto flex items-center gap-1.5">
          {isRunning && <span className="text-[11px] text-muted-foreground">执行中…</span>}
          <ExitBadge code={item.exitCode} />
        </span>
      </summary>
      {item.output && (
        <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 text-[11px]">
          {item.output}
        </pre>
      )}
      {!item.output && isRunning && (
        <p className="mt-2 text-[11px] text-muted-foreground">等待输出…</p>
      )}
    </details>
  );
}

function renderFileChange(item: Extract<ItemView, { kind: "fileChange" }>) {
  return (
    <div key={item.id} className="self-start max-w-[92%] rounded-lg border bg-card px-3 py-2 text-xs">
      <p className="font-medium">文件变更（{item.files.length}） · {item.status}</p>
      {item.files.length === 0 && <p className="mt-1 text-[11px] text-muted-foreground">（无文件）</p>}
      {item.files.map((f) => (
        <div key={f.path} className="mt-2">
          <p className="font-mono text-[11px]">
            {f.path} · {f.kind}
          </p>
          {f.diff ? (
            <div className="mt-1">
              {/* 复用 DiffView：统一 diff 文本按 diff-rows 渲染 */}
              <DiffView localText="" remoteText={f.diff} localLabel="变更前" remoteLabel="变更后" />
              {/* 兜底：若 diff 超长，DiffView 内部会滚动；同时保留原始文本预览 */}
              <details className="mt-1">
                <summary className="cursor-pointer text-[11px] text-muted-foreground">查看原始 diff</summary>
                <pre className="mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 text-[11px]">
                  {f.diff.slice(0, 8000)}
                </pre>
              </details>
            </div>
          ) : (
            <p className="mt-1 text-[11px] text-muted-foreground">（无 diff）</p>
          )}
        </div>
      ))}
    </div>
  );
}

function renderPlan(item: Extract<ItemView, { kind: "plan" }>) {
  // plan 文本可能为 markdown checklist，简化为按行渲染 checkbox
  const lines = item.text.split("\n");
  const hasChecklist = lines.some((l) => /^\s*[-*]\s+\[[ xX]\]/.test(l));
  if (hasChecklist) {
    return (
      <div key={item.id} className="self-start max-w-[92%] rounded-lg border bg-card px-3 py-2 text-xs">
        <p className="font-medium">计划</p>
        <ul className="mt-1 space-y-0.5">
          {lines.map((line, idx) => {
            const m = line.match(/^\s*[-*]\s+\[([ xX])\]\s*(.*)$/);
            if (m) {
              const checked = m[1].toLowerCase() === "x";
              return (
                <li key={idx} className="flex gap-1.5">
                  <input type="checkbox" checked={checked} readOnly className="mt-0.5" />
                  <span className={checked ? "text-muted-foreground line-through" : ""}>{m[2]}</span>
                </li>
              );
            }
            if (!line.trim()) return null;
            return (
              <li key={idx} className="whitespace-pre-wrap break-words">
                {line}
              </li>
            );
          })}
        </ul>
      </div>
    );
  }
  return (
    <div key={item.id} className="self-start max-w-[92%] rounded-lg border bg-card px-3 py-2 text-xs">
      <p className="font-medium">计划</p>
      <pre className="mt-1 whitespace-pre-wrap break-words text-xs">{item.text}</pre>
    </div>
  );
}

function renderUnknown(item: Extract<ItemView, { kind: "unknown" }>) {
  return (
    <details key={item.id} className="self-start max-w-[92%] rounded-lg border bg-muted/30 px-3 py-2 text-xs">
      <summary className="cursor-pointer text-muted-foreground">未知消息 {item.id}</summary>
      <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-[11px]">
        {JSON.stringify((item as { raw: unknown }).raw, null, 2)}
      </pre>
    </details>
  );
}

export function MessageStream({ items, activeTurnId, onInsertToNote }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null);

  // 有新消息时滚动到底（由外层 listRef 控制更好，这里保留一个可观测的 bottom anchor）
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [items.length]);

  if (items.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">
        输入消息开始与 Agent 对话。Agent 可读写当前 vault 并执行命令。
      </p>
    );
  }

  return (
    <>
      {items.map((item) => {
        switch (item.kind) {
          case "userMessage":
            return renderUserMessage(item);
          case "agentMessage":
            return renderAgentMessage(item, activeTurnId, onInsertToNote);
          case "reasoning":
            return renderReasoning(item);
          case "commandExecution":
            return renderCommandExecution(item);
          case "fileChange":
            return renderFileChange(item);
          case "plan":
            return renderPlan(item);
          case "unknown":
            return renderUnknown(item);
          default:
            return renderUnknown(item as Extract<ItemView, { kind: "unknown" }>);
        }
      })}
      <div ref={bottomRef} />
    </>
  );
}
