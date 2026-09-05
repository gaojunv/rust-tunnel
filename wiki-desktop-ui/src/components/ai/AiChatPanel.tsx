/**
 * AI 聊天面板 —— 流式对话、模型选择、携带笔记上下文、插入到笔记
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Send, Square, Settings2, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { chatStream, listRelayModels, type ChatMessage } from "@/lib/ai-client";
import { buildChatMessages } from "@/lib/ai-prompts";
import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { getAiModel, setAiModel, AI_MODEL_KEY } from "@/lib/ai-config";
import type { NoteDto } from "@/api/types";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";

type Props = {
  onInsert: (text: string) => void;
  getCurrentNote: () => Promise<NoteDto | null>;
  onOpenSettings: () => void;
};

export function AiChatPanel({ onInsert, getCurrentNote, onOpenSettings }: Props) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [withNote, setWithNote] = useState(true);
  const [models, setModels] = useState<string[]>([]);
  const [model, setModel] = useState<string | null>(() => getAiModel());
  const [modelError, setModelError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // 拉模型列表
  const refreshModels = useCallback(async () => {
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl) {
      setModelError("未连接服务器");
      setModels([]);
      return;
    }
    const token = getToken(cfg.baseUrl);
    if (!token) {
      setModelError("未连接服务器");
      setModels([]);
      return;
    }
    setModelError(null);
    try {
      const list = await listRelayModels(cfg.baseUrl);
      setModels(list);
      // 若本地无选中且列表非空，默认选第一项
      if (!model && list.length > 0) {
        setModel(list[0]);
        setAiModel(list[0]);
      } else if (model && list.length > 0 && !list.includes(model)) {
        // 已选不在列表中，保持原值
      }
      if (list.length === 0) setModelError("无可用模型");
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setModelError(msg || "加载模型失败");
    }
  }, [model]);

  useEffect(() => {
    void refreshModels();
  }, [refreshModels]);

  // 监听 storage 变化（设置页登录后）可手动重拉；此处简单在 mount 拉一次

  const handleModelChange = (v: string) => {
    setModel(v);
    try {
      localStorage.setItem(AI_MODEL_KEY, v);
    } catch {
      // 忽略
    }
  };

  const handleStop = useCallback(() => {
    abortRef.current?.abort();
    setSending(false);
  }, []);

  // Esc 中止
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape" && sending) {
        e.preventDefault();
        handleStop();
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [sending, handleStop]);

  const hasConfig = (() => {
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl) return false;
    const token = getToken(cfg.baseUrl);
    return !!token && !!model;
  })();

  const handleSend = useCallback(async () => {
    const text = input.trim();
    if (!text || sending) return;
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl) {
      setError("未配置服务器，请先打开设置");
      return;
    }
    const token = getToken(cfg.baseUrl);
    if (!token) {
      setError("未登录，请先打开设置完成登录");
      return;
    }
    if (!model) {
      setError("请选择模型");
      return;
    }
    setError(null);
    const userMsg: ChatMessage = { role: "user", content: text };
    // 计算携带笔记上下文
    let noteTitle: string | undefined;
    let noteBody: string | undefined;
    if (withNote) {
      try {
        const note = await getCurrentNote();
        if (note) {
          noteTitle = note.title;
          noteBody = note.body;
        }
      } catch {
        // 忽略，降级为无上下文
      }
    }
    const historyWithUser = [...messages, userMsg];
    const toSend = buildChatMessages({ history: historyWithUser, noteTitle, noteBody });

    setMessages((prev) => [...prev, userMsg, { role: "assistant", content: "" }]);
    setInput("");
    setSending(true);
    const ac = new AbortController();
    abortRef.current = ac;
    let acc = "";
    try {
      for await (const delta of chatStream({
        baseUrl: cfg.baseUrl,
        model,
        messages: toSend,
        signal: ac.signal,
      })) {
        acc += delta;
        setMessages((prev) => {
          const next = [...prev];
          const last = next[next.length - 1];
          if (last && last.role === "assistant") {
            next[next.length - 1] = { ...last, content: acc };
          }
          return next;
        });
      }
    } catch (e: unknown) {
      if ((e as Error)?.name === "AbortError" || ac.signal.aborted) {
        // 中止不算错误
      } else {
        const msg = e instanceof Error ? e.message : String(e);
        setError(msg);
        // 若流已产出部分内容，保留；否则移除空 assistant 占位
        if (!acc) {
          setMessages((prev) => prev.slice(0, -1));
        }
      }
    } finally {
      abortRef.current = null;
      setSending(false);
      // 滚动到底部
      requestAnimationFrame(() => {
        if (listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
      });
    }
  }, [input, sending, model, withNote, messages, getCurrentNote]);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void handleSend();
    }
  };

  // 未配置引导
  const cfg = loadSyncConfig();
  const needsSetup = !cfg?.baseUrl || !getToken(cfg?.baseUrl ?? "");

  if (needsSetup) {
    return (
      <div className="flex h-full flex-col gap-3 p-4">
        <div className="rounded-lg border border-dashed bg-muted/30 p-4">
          <p className="text-sm font-medium">未配置服务器</p>
          <p className="mt-1 text-xs text-muted-foreground">配置服务器地址并登录后即可使用 AI 助手。</p>
          <Button type="button" size="sm" className="mt-3" onClick={onOpenSettings}>
            <Settings2 className="size-3.5" />
            打开设置
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col gap-2 p-3">
      {/* 模型选择 */}
      <div className="flex items-center gap-2">
        <select
          value={model ?? ""}
          onChange={(e) => handleModelChange(e.target.value)}
          className="flex h-8 min-w-0 flex-1 rounded-md border border-input bg-background px-2 text-xs"
        >
          <option value="">{models.length === 0 ? "无模型" : "选择模型"}</option>
          {models.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
        <Button type="button" variant="ghost" size="icon" className="size-8 shrink-0" onClick={() => void refreshModels()} title="刷新模型">
          <Settings2 className="size-3.5" />
        </Button>
      </div>
      {modelError && (
        <div className="flex items-center gap-2 rounded-md border border-amber-200 bg-amber-50 px-2 py-1.5 text-xs text-amber-800 dark:border-amber-900 dark:bg-amber-950/30 dark:text-amber-200">
          <span className="flex-1">{modelError}</span>
          <Button type="button" size="sm" variant="outline" className="h-7 text-xs" onClick={onOpenSettings}>
            打开设置
          </Button>
        </div>
      )}
      <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <input type="checkbox" checked={withNote} onChange={(e) => setWithNote(e.target.checked)} />
        携带当前笔记
      </label>

      {/* 消息列表 */}
      <div ref={listRef} className="flex min-h-0 flex-1 flex-col gap-3 overflow-auto rounded-md border bg-card p-3">
        {messages.length === 0 ? (
          <p className="text-xs text-muted-foreground">输入问题开始对话，AI 会参考当前笔记内容作答。</p>
        ) : (
          messages.map((m, idx) => (
            <div key={idx} className={m.role === "user" ? "self-end max-w-[85%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground" : "self-start max-w-[92%] rounded-lg bg-muted px-3 py-2 text-sm"}>
              {m.role === "user" ? (
                <span className="whitespace-pre-wrap break-words">{m.content}</span>
              ) : (
                <>
                  <div className="prose prose-sm max-w-none dark:prose-invert">
                    <Streamdown>{m.content || (sending && idx === messages.length - 1 ? "…" : "")}</Streamdown>
                  </div>
                  {m.content && (
                    <div className="mt-2">
                      <Button type="button" variant="outline" size="sm" className="h-7 text-xs" onClick={() => onInsert(m.content)}>
                        插入到笔记
                      </Button>
                    </div>
                  )}
                </>
              )}
            </div>
          ))
        )}
        {error && <p className="rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{error}</p>}
      </div>

      {/* 输入区 */}
      <div className="flex gap-2">
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={hasConfig ? "输入消息，Enter 发送，Shift+Enter 换行" : "请先选择模型"}
          disabled={!hasConfig || sending}
          className="min-h-[44px] max-h-28 flex-1 resize-none rounded-md border border-input bg-background px-3 py-2 text-sm placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50"
          rows={2}
        />
        {sending ? (
          <Button type="button" variant="secondary" size="icon" className="size-9 shrink-0" onClick={handleStop} aria-label="停止">
            <Square className="size-3.5" />
          </Button>
        ) : (
          <Button type="button" size="icon" className="size-9 shrink-0" onClick={() => void handleSend()} disabled={!input.trim() || !hasConfig} aria-label="发送">
            <Send className="size-4" />
          </Button>
        )}
      </div>
      {messages.length > 0 && (
        <div className="flex justify-end">
          <Button type="button" variant="ghost" size="sm" className="h-7 text-xs text-muted-foreground" onClick={() => setMessages([])}>
            <Trash2 className="size-3.5" />
            清空对话
          </Button>
        </div>
      )}
    </div>
  );
}
