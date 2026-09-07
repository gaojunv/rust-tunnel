/**
 * MessageStream —— Codex CLI 风格紧凑消息流 + 自绘滚动统一
 * - userMessage：› 前缀弱色、去气泡
 * - agentMessage：全宽 Streamdown、hover 小按钮
 * - reasoning：• 思考 单行斜体+折叠
 * - commandExecution：• exec <摘要> + 徽标，输出区限高 ScrollArea
 * - fileChange：• edit <文件> + 计数，DiffView 保持
 * - plan/unknown：保留 Checklist/JSON
 * - error：红色左边条横幅
 * - system：居中弱色分隔线（info 弱化 / warn 警示色），历史回填分隔等
 */
import { useEffect, useRef, useState } from "react";
import { ScrollArea } from "@/components/ui/scroll-area";
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
      className={`inline-flex shrink-0 items-center rounded-full border px-1.5 py-0.5 text-[10px] font-medium ${
        ok
          ? "border-emerald-500/20 bg-emerald-500/12 text-emerald-600 dark:text-emerald-300"
          : "border-red-500/25 bg-red-500/15 text-red-600 dark:text-red-300"
      }`}
    >
      {ok ? "✓" : "✗"} {ok ? "退出码 0" : `退出码 ${code}`}
    </span>
  );
}

function commandSummary(full: string): string {
  if (!full) return "(空命令)";
  // 取首行 + 截断
  const first = full.split("\n")[0].trim();
  if (first.length > 72) return `${first.slice(0, 72)}…`;
  return first;
}

function diffStats(diff: string): { adds: number; dels: number } | null {
  if (!diff) return null;
  const lines = diff.split("\n");
  let adds = 0;
  let dels = 0;
  for (const l of lines) {
    if (l.startsWith("+++") || l.startsWith("---")) continue;
    if (l.startsWith("+")) adds += 1;
    else if (l.startsWith("-")) dels += 1;
  }
  if (adds === 0 && dels === 0) return null;
  return { adds, dels };
}

function renderUserMessage(item: Extract<ItemView, { kind: "userMessage" }>) {
  return (
    <div key={item.id} className="w-full py-1">
      <div className="flex gap-2 text-sm leading-relaxed">
        <span className="shrink-0 select-none font-mono text-muted-foreground">›</span>
        <span className="min-w-0 flex-1 whitespace-pre-wrap break-words text-foreground/85">
          {item.text}
        </span>
      </div>
    </div>
  );
}

function renderAgentMessage(
  item: Extract<ItemView, { kind: "agentMessage" }>,
  activeTurnId: string | null | undefined,
  onInsertToNote: (text: string) => void,
) {
  return (
    <div key={item.id} className="group w-full py-1">
      <div className="prose prose-sm max-w-none dark:prose-invert [&_p]:my-2 [&_pre]:my-2">
        <Streamdown>{item.text || (activeTurnId ? "…" : "")}</Streamdown>
      </div>
      {item.complete && item.text && (
        <div className="mt-1 hidden group-hover:block">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-[11px] text-muted-foreground hover:text-foreground"
            onClick={() => onInsertToNote(item.text)}
          >
            插入到笔记
          </Button>
        </div>
      )}
    </div>
  );
}

function ReasoningItem({ item }: { item: Extract<ItemView, { kind: "reasoning" }> }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="w-full py-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="inline-flex items-center gap-1.5 text-xs italic text-muted-foreground hover:text-foreground"
        aria-expanded={open}
      >
        <span className="font-mono not-italic">•</span> 思考
        <span className="text-[11px] not-italic">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <pre className="mt-1.5 max-h-[32vh] overflow-auto whitespace-pre-wrap break-words rounded-md border bg-muted/40 px-2.5 py-2 text-[11px] leading-relaxed">
          {item.text || "（空）"}
        </pre>
      )}
    </div>
  );
}

function CommandExecutionItem({ item }: { item: Extract<ItemView, { kind: "commandExecution" }> }) {
  const isRunning = item.status === "inProgress" || item.status === "running";
  const [open, setOpen] = useState(isRunning);
  useEffect(() => {
    if (isRunning) setOpen(true);
  }, [isRunning]);
  const summary = commandSummary(item.command);
  return (
    <div className="w-full border-y border-border/40 bg-card/40 py-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-1 py-0.5 text-left font-mono text-xs hover:bg-accent/30"
      >
        <span className="shrink-0 select-none text-muted-foreground">•</span>
        <span className="text-muted-foreground">exec</span>
        <span className="min-w-0 flex-1 truncate" title={item.command}>
          {summary}
        </span>
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {isRunning && <span className="text-[11px] text-muted-foreground">执行中…</span>}
          <span className="text-[11px] text-muted-foreground">{open ? "▾" : "▸"}</span>
          <ExitBadge code={item.exitCode} />
        </span>
      </button>
      {open && (
        <div className="mt-1 px-1">
          {item.output ? (
            <ScrollArea className="max-h-64 overflow-hidden rounded-md border bg-muted/35">
              <pre className="whitespace-pre-wrap break-words px-2.5 py-2 font-mono text-[11px] leading-relaxed">
                {item.output}
              </pre>
            </ScrollArea>
          ) : isRunning ? (
            <p className="px-2 py-1.5 text-[11px] text-muted-foreground">等待输出…</p>
          ) : null}
        </div>
      )}
    </div>
  );
}

function FileChangeItem({ item }: { item: Extract<ItemView, { kind: "fileChange" }> }) {
  const [open, setOpen] = useState(true);
  return (
    <div className="w-full border-y border-border/40 bg-card/40 py-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-1 py-0.5 text-left text-xs hover:bg-accent/30"
      >
        <span className="shrink-0 select-none font-mono text-muted-foreground">•</span>
        <span className="font-mono text-muted-foreground">edit</span>
        <span className="min-w-0 flex-1 truncate" title={item.files.map((f) => f.path).join(", ")}>
          {item.files.length === 0 ? "（无文件）" : item.files.map((f) => f.path).join(", ")}
        </span>
        {/* diff 统计汇总：跨所有文件 */}
        {(() => {
          const allDiff = item.files.map((f) => f.diff).join("\n");
          const st = diffStats(allDiff);
          if (!st) return null;
          return (
            <span className="shrink-0 font-mono text-[11px]">
              <span className="text-emerald-600 dark:text-emerald-400">+{st.adds}</span>
              <span className="mx-0.5 text-muted-foreground">·</span>
              <span className="text-red-600 dark:text-red-400">-{st.dels}</span>
            </span>
          );
        })()}
        <span className="shrink-0 text-[11px] text-muted-foreground">{item.status}</span>
        <span className="shrink-0 text-[11px] text-muted-foreground">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <div className="mt-1 px-1">
          {item.files.length === 0 ? (
            <p className="px-2 py-1 text-[11px] text-muted-foreground">（无文件）</p>
          ) : (
            <div className="space-y-2">
              {item.files.map((f) => (
                <div key={f.path} className="overflow-hidden rounded-md border">
                  <div className="flex items-center gap-2 border-b bg-muted/30 px-2 py-1 text-[11px]">
                    <span className="min-w-0 flex-1 truncate font-mono">{f.path}</span>
                    <span className="shrink-0 rounded bg-muted px-1 py-0.5 text-[10px]">{f.kind}</span>
                    {(() => {
                      const st = diffStats(f.diff);
                      if (!st) return null;
                      return (
                        <span className="shrink-0 font-mono text-[10px]">
                          <span className="text-emerald-600 dark:text-emerald-400">+{st.adds}</span>
                          <span className="mx-0.5 text-muted-foreground">·</span>
                          <span className="text-red-600 dark:text-red-400">-{st.dels}</span>
                        </span>
                      );
                    })()}
                  </div>
                  {f.diff ? (
                    <div className="p-1">
                      <DiffView
                        localText=""
                        remoteText={f.diff}
                        localLabel="变更前"
                        remoteLabel="变更后"
                      />
                      <details className="mt-2">
                        <summary className="cursor-pointer px-1 text-[11px] text-muted-foreground">
                          查看原始 diff
                        </summary>
                        <ScrollArea className="mt-1 max-h-64 overflow-hidden rounded bg-muted">
                          <pre className="whitespace-pre-wrap break-words px-2.5 py-2 font-mono text-[11px]">
                            {f.diff.slice(0, 16000)}
                          </pre>
                        </ScrollArea>
                      </details>
                    </div>
                  ) : (
                    <p className="px-3 py-2 text-[11px] text-muted-foreground">（无 diff）</p>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function PlanItem({ item }: { item: Extract<ItemView, { kind: "plan" }> }) {
  const [open, setOpen] = useState(true);
  const lines = item.text.split("\n");
  const hasChecklist = lines.some((l) => /^\s*[-*]\s+\[[ xX]\]/.test(l));
  if (hasChecklist) {
    return (
      <div className="w-full py-1">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="inline-flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground"
        >
          <span className="font-mono">•</span> 计划
          <span className="text-[11px]">{open ? "▾" : "▸"}</span>
        </button>
        {open && (
          <ul className="mt-1 space-y-0.5 border-l-2 border-border/60 pl-3 text-xs">
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
        )}
      </div>
    );
  }
  return (
    <div className="w-full py-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="inline-flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground"
      >
        <span className="font-mono">•</span> 计划
        <span className="text-[11px]">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <pre className="mt-1.5 max-h-[32vh] overflow-auto whitespace-pre-wrap break-words rounded-md border bg-muted/35 px-2.5 py-2 text-xs">
          {item.text}
        </pre>
      )}
    </div>
  );
}

function renderError(item: Extract<ItemView, { kind: "error" }>) {
  return (
    <div
      key={item.id}
      className="my-1 w-full overflow-hidden rounded-md border border-red-500/30 bg-red-500/10 text-xs"
      role="alert"
    >
      <div className="flex gap-2">
        <span className="w-1 shrink-0 self-stretch bg-red-500/60" aria-hidden />
        <div className="min-w-0 flex-1 py-1.5 pr-2">
          <p className="font-medium text-red-700 dark:text-red-300">错误</p>
          <pre className="mt-1 whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-red-800 dark:text-red-200">
            {item.message}
          </pre>
        </div>
      </div>
    </div>
  );
}

function SystemItem({ item }: { item: Extract<ItemView, { kind: "system" }> }) {
  // 居中弱色分隔线；tone=warn（warning/configWarning 等）用警示色，info 用弱化小字
  const warn = item.tone === "warn";
  return (
    <div
      key={item.id}
      className={`flex w-full select-none items-center gap-2 py-1.5 ${
        warn ? "text-amber-700 dark:text-amber-300" : "text-muted-foreground"
      }`}
      aria-live="polite"
    >
      <span aria-hidden className={`h-px min-w-0 flex-1 ${warn ? "bg-amber-500/40" : "bg-border/60"}`} />
      <span className="min-w-0 shrink truncate text-center text-[11px] leading-none">{item.text}</span>
      <span aria-hidden className={`h-px min-w-0 flex-1 ${warn ? "bg-amber-500/40" : "bg-border/60"}`} />
    </div>
  );
}

function UnknownItem({ item }: { item: Extract<ItemView, { kind: "unknown" }> }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="w-full border-y border-border/30 py-1">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="text-xs text-muted-foreground hover:text-foreground"
      >
        未知消息 {item.id} {open ? "▾" : "▸"}
      </button>
      {open && (
        <pre className="mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 text-[11px]">
          {JSON.stringify((item as { raw: unknown }).raw, null, 2)}
        </pre>
      )}
    </div>
  );
}

export function MessageStream({ items, activeTurnId, onInsertToNote }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [items.length]);

  if (items.length === 0) {
    return (
      <p className="px-1 py-2 text-xs text-muted-foreground">输入消息开始与 Agent 对话。Agent 可读写当前 vault 并执行命令。</p>
    );
  }

  return (
    <div className="divide-y divide-border/25">
      {items.map((item) => {
        switch (item.kind) {
          case "userMessage":
            return renderUserMessage(item);
          case "agentMessage":
            return renderAgentMessage(item, activeTurnId, onInsertToNote);
          case "reasoning":
            return <ReasoningItem key={item.id} item={item} />;
          case "commandExecution":
            return <CommandExecutionItem key={item.id} item={item} />;
          case "fileChange":
            return <FileChangeItem key={item.id} item={item} />;
          case "plan":
            return <PlanItem key={item.id} item={item} />;
          case "error":
            return renderError(item);
          case "system":
            return <SystemItem key={item.id} item={item} />;
          case "unknown":
            return <UnknownItem key={item.id} item={item} />;
          // ItemView 各 kind 已全部覆盖，穷尽 switch；新增 kind 时此处会缺返回分支而编译报错
        }
      })}
      <div ref={bottomRef} />
    </div>
  );
}
