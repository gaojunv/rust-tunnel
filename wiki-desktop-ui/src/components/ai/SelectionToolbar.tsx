import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SELECTION_ACTIONS, type SelectionAction } from "@/lib/ai-prompts";
import { buildSelectionMessages } from "@/lib/ai-prompts";
import { chatStream } from "@/lib/ai-client";
import { getAiConfig } from "@/lib/ai-config";
import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import type { SelectionSource } from "@/lib/selection-source";

type Props = {
  source: SelectionSource;
  containerRef: React.RefObject<HTMLDivElement>;
  noteTitle: string;
  noteBody: string;
  noteKey: string | null;
  onOpenSettings: () => void;
};

export function SelectionToolbar({
  source,
  containerRef,
  noteTitle,
  noteBody,
  noteKey,
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
    const sel = source.getSelection();
    if (!sel) {
      setVisible(false);
      setSelection(null);
      return;
    }
    setSelection(sel);
    try {
      const caret = source.getCaretRect(sel.end);
      if (caret) {
        const cRect = containerRef.current?.getBoundingClientRect();
        let top: number;
        let left: number;
        if (cRect && containerRef.current) {
          // caret is already container-relative from the adapter
          top = caret.top - 36;
          left = caret.left;
          const clampedLeft = Math.max(4, Math.min(left, containerRef.current.clientWidth - 160));
          const clampedTop = Math.max(4, top);
          setPos({ top: clampedTop, left: clampedLeft });
        } else {
          setPos({ top: caret.top - 36, left: caret.left });
        }
      } else {
        setPos(null);
      }
    } catch {
      setPos(null);
    }
    setVisible(true);
  }, [source, containerRef]);

  const handleSelect = useCallback(() => {
    requestAnimationFrame(() => updateSelection());
  }, [updateSelection]);

  // polling-based selection detection — CM does not emit textarea select/mouseup events reliably
  // requestAnimationFrame polling is cheap and torn down when selection is stable
  useEffect(() => {
    let raf: number | null = null;
    let lastKey = "";
    const tick = () => {
      const sel = source.getSelection();
      const key = sel ? `${sel.start}:${sel.end}` : "";
      if (key !== lastKey) {
        lastKey = key;
        handleSelect();
      }
      raf = requestAnimationFrame(tick);
    };
    // use mouseup/keyup as cheaper triggers plus a light interval fallback
    const onAny = () => handleSelect();
    window.addEventListener("mouseup", onAny);
    window.addEventListener("keyup", onAny);
    // lightweight poll only while component mounted — rAF but throttled via key check so no visual jitter
    raf = requestAnimationFrame(tick);
    const interval = window.setInterval(() => {
      const sel = source.getSelection();
      const key = sel ? `${sel.start}:${sel.end}` : "";
      if (key !== lastKey) {
        lastKey = key;
        updateSelection();
      }
    }, 300);
    return () => {
      window.removeEventListener("mouseup", onAny);
      window.removeEventListener("keyup", onAny);
      if (raf !== null) cancelAnimationFrame(raf);
      clearInterval(interval);
    };
  }, [source, handleSelect, updateSelection]);

  // reset on note switch
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
          // aborted
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

  // Esc
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

  const handleReplace = useCallback(() => {
    if (!selection || !result) return;
    source.replaceRange(selection.start, selection.end, result);
    setResultOpen(false);
  }, [selection, result, source]);

  const handleInsertAfter = useCallback(() => {
    if (!selection || !result) return;
    source.insertAt(selection.end, "\n" + result);
    setResultOpen(false);
  }, [selection, result, source]);

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
            <Button type="button" size="sm" className="h-7 text-xs" disabled={!result || streaming} onClick={handleReplace}>
              替换选区
            </Button>
            <Button type="button" variant="outline" size="sm" className="h-7 text-xs" disabled={!result || streaming} onClick={handleInsertAfter}>
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
