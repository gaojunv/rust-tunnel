/**
 * 选区浮动工具条 —— 挂在 NoteEditor 编辑态，随选区出现，mirror-div 定位
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SELECTION_ACTIONS, type SelectionAction } from "@/lib/ai-prompts";
import { buildSelectionMessages } from "@/lib/ai-prompts";
import { chatStream } from "@/lib/ai-client";
import { getAiConfig } from "@/lib/ai-config";
import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { measureCaretInTextarea } from "@/lib/caret-position";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";

type Props = {
  textareaRef: React.RefObject<HTMLTextAreaElement>;
  containerRef: React.RefObject<HTMLDivElement>;
  noteTitle: string;
  noteBody: string;
  noteKey: string | null;
  onReplaceSelection: (text: string) => void;
  onInsertAfterSelection: (text: string) => void;
  onOpenSettings: () => void;
};

export function SelectionToolbar({
  textareaRef,
  containerRef,
  noteTitle,
  noteBody,
  noteKey,
  onReplaceSelection,
  onInsertAfterSelection,
  onOpenSettings,
}: Props) {
  const [visible, setVisible] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const [selection, setSelection] = useState<{ text: string; start: number; end: number } | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const [resultOpen, setResultOpen] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const [error, setError] = useState<string | null>(null);

  const updateSelection = useCallback(() => {
    const ta = textareaRef.current;
    if (!ta) {
      setVisible(false);
      setSelection(null);
      return;
    }
    const start = ta.selectionStart ?? 0;
    const end = ta.selectionEnd ?? 0;
    if (start === end) {
      setVisible(false);
      setSelection(null);
      return;
    }
    const text = ta.value.slice(start, end);
    if (!text.trim()) {
      setVisible(false);
      setSelection(null);
      return;
    }
    setSelection({ text, start, end });
    // 定位：mirror-div 法，相对容器
    try {
      const caret = measureCaretInTextarea(ta, end);
      const container = containerRef.current;
      if (container) {
        const cRect = container.getBoundingClientRect();
        const taRect = ta.getBoundingClientRect();
        // caret 相对 textarea 内容区，换算到容器坐标
        const top = taRect.top - cRect.top + caret.top - 36;
        const left = taRect.left - cRect.left + caret.left;
        // 边界收敛：尽量不超出容器
        const clampedLeft = Math.max(4, Math.min(left, container.clientWidth - 160));
        const clampedTop = Math.max(4, top);
        setPos({ top: clampedTop, left: clampedLeft });
      } else {
        setPos({ top: caret.top - 36, left: caret.left });
      }
    } catch {
      setPos(null);
    }
    setVisible(true);
  }, [textareaRef, containerRef]);

  const handleSelect = useCallback(() => {
    // 选区变化后下一帧再测，避免 selectionStart 尚未更新
    requestAnimationFrame(() => updateSelection());
  }, [updateSelection]);

  // 监听 textarea 事件
  useEffect(() => {
    const ta = textareaRef.current;
    if (!ta) return;
    const onMouseUp = () => handleSelect();
    const onKeyUp = () => handleSelect();
    const onSelect = () => handleSelect();
    const onBlur = () => {
      // 失焦延迟隐藏，避免点击工具条时先失焦
      setTimeout(() => {
        const active = document.activeElement;
        // 若焦点仍在工具条内则不隐藏
        if (active && containerRef.current?.contains(active as Node)) return;
        // 否则若仍有选区则保留，切换笔记时由 noteKey 变化隐藏
      }, 150);
    };
    ta.addEventListener("mouseup", onMouseUp);
    ta.addEventListener("keyup", onKeyUp);
    ta.addEventListener("select", onSelect);
    ta.addEventListener("blur", onBlur);
    return () => {
      ta.removeEventListener("mouseup", onMouseUp);
      ta.removeEventListener("keyup", onKeyUp);
      ta.removeEventListener("select", onSelect);
      ta.removeEventListener("blur", onBlur);
    };
  }, [textareaRef, containerRef, handleSelect]);

  // 切换笔记隐藏
  useEffect(() => {
    setVisible(false);
    setSelection(null);
    setResult(null);
    setResultOpen(false);
    setError(null);
    abortRef.current?.abort();
    setStreaming(false);
  }, [noteKey]);

  const handleAction = useCallback(
    async (action: SelectionAction) => {
      if (!selection) return;
      const cfg = loadSyncConfig();
      if (!cfg?.baseUrl || !getToken(cfg.baseUrl)) {
        setError("未配置服务器");
        onOpenSettings();
        return;
      }
      const aiCfg = getAiConfig();
      if (!aiCfg) {
        setError("未选择模型，请先在 AI 助手面板选择模型");
        onOpenSettings();
        return;
      }
      setError(null);
      setResult("");
      setResultOpen(true);
      setStreaming(true);
      const ac = new AbortController();
      abortRef.current = ac;
      const messages = buildSelectionMessages({
        action,
        selection: selection.text,
        noteTitle,
        noteBody,
      });
      let acc = "";
      try {
        for await (const delta of chatStream({
          baseUrl: aiCfg.baseUrl,
          model: aiCfg.model,
          messages,
          signal: ac.signal,
        })) {
          acc += delta;
          setResult(acc);
        }
      } catch (e: unknown) {
        if ((e as Error)?.name === "AbortError" || ac.signal.aborted) {
          // 中止
        } else {
          const msg = e instanceof Error ? e.message : String(e);
          setError(msg);
          if (!acc) setResult(null);
        }
      } finally {
        abortRef.current = null;
        setStreaming(false);
      }
    },
    [selection, noteTitle, noteBody, onOpenSettings],
  );

  // Esc 关闭结果/中止流
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (streaming) {
          abortRef.current?.abort();
          setStreaming(false);
        } else if (resultOpen) {
          setResultOpen(false);
        } else if (visible) {
          setVisible(false);
        }
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [streaming, resultOpen, visible]);

  if (!visible || !selection) return null;

  return (
    <>
      <div
        className="absolute z-20 flex items-center gap-1 rounded-full border bg-popover px-1.5 py-1 shadow-lg"
        style={pos ? { top: pos.top, left: pos.left } : { top: 8, left: 8 }}
        role="toolbar"
        aria-label="选区 AI 操作"
      >
        {SELECTION_ACTIONS.map((a) => (
          <Button
            key={a.id}
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 rounded-full px-2.5 text-xs"
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => void handleAction(a.id)}
            disabled={streaming}
          >
            {a.label}
          </Button>
        ))}
        <Button type="button" variant="ghost" size="icon" className="size-7 rounded-full" onMouseDown={(e) => e.preventDefault()} onClick={() => setVisible(false)} aria-label="关闭">
          <X className="size-3.5" />
        </Button>
      </div>
      {resultOpen && (
        <div className="absolute z-20 max-h-64 w-[min(420px,95%)] overflow-auto rounded-lg border bg-popover p-3 shadow-xl" style={pos ? { top: (pos.top ?? 0) + 40, left: pos.left ?? 8 } : { top: 48, left: 8 }}>
          <div className="mb-2 flex items-center justify-between">
            <span className="text-xs font-medium">AI 结果 {streaming ? "（生成中…）" : ""}</span>
            <Button type="button" variant="ghost" size="icon" className="size-6" onClick={() => setResultOpen(false)} aria-label="关闭">
              <X className="size-3.5" />
            </Button>
          </div>
          {error && <p className="mb-2 rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{error}</p>}
          {result !== null && result !== "" ? (
            <div className="prose prose-sm max-w-none text-sm dark:prose-invert">
              <Streamdown>{result}</Streamdown>
            </div>
          ) : streaming ? (
            <p className="text-xs text-muted-foreground">生成中…</p>
          ) : (
            <p className="text-xs text-muted-foreground">暂无结果</p>
          )}
          <div className="mt-3 flex gap-2">
            <Button type="button" size="sm" className="h-7 text-xs" disabled={!result || streaming} onClick={() => { if (result) onReplaceSelection(result); setResultOpen(false); }}>
              替换选区
            </Button>
            <Button type="button" variant="outline" size="sm" className="h-7 text-xs" disabled={!result || streaming} onClick={() => { if (result) onInsertAfterSelection("\n" + result); setResultOpen(false); }}>
              插入其后
            </Button>
            <Button type="button" variant="ghost" size="sm" className="h-7 text-xs" onClick={() => setResultOpen(false)}>
              关闭
            </Button>
          </div>
        </div>
      )}
    </>
  );
}
