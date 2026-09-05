import { useCallback, useEffect, useImperativeHandle, useRef, useState, forwardRef, useMemo } from "react";
import { Eye, Pencil, Save, Trash2, FileText, FilePenLine, Sparkles } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { OverlayScrollbar } from "@/components/ui/scroll-area";
import { getNote, saveNote, deleteNote, renameNote, listNotes, saveAttachment } from "@/api/tauri";
import type { NoteDto, NoteSummary } from "@/api/types";
import { MarkdownPreview } from "@/components/MarkdownPreview";
import { NoteFormDialog } from "@/components/NoteFormDialog";
import { normalizeNoteKey, validateNoteKey } from "@/lib/note-key";
import { SelectionToolbar } from "@/components/ai/SelectionToolbar";
import { LinkSuggestDialog } from "@/components/ai/LinkSuggestDialog";
import { MarkdownEditor, type MarkdownEditorHandle } from "@/components/editor/MarkdownEditor";
import { EditorToolbar } from "@/components/editor/EditorToolbar";
import { EditorView } from "@codemirror/view";
import type { SelectionSource } from "@/lib/selection-source";
import { readScrollPos, writeScrollPos } from "@/lib/scroll-memory";

export interface NoteEditorHandle {
  insertAtCursor(text: string): void;
  replaceSelection(text: string): void;
  getSelection(): { text: string; start: number; end: number } | null;
  scrollToLine(line: number): void;
  appendToBody(text: string): void;
  getBody(): string;
  getTitle(): string;
  getEditorView(): EditorView | null;
  flushSave(): Promise<void>;
}

type Props = {
  noteKey: string | null;
  mode: "edit" | "preview";
  onModeChange: (m: "edit" | "preview") => void;
  onSaved: () => void;
  onDeleted: (deletedKey?: string) => void;
  onDirtyChange: (dirty: boolean) => void;
  onNavigate?: (key: string) => void;
  onCreate?: (key: string) => void;
  onRenamed?: (oldKey: string, newKey: string) => void;
  onOpenSettings?: () => void;
  refreshToken?: number;
  previewContainerRef?: React.RefObject<HTMLDivElement | null>;
};

function isNotFoundError(msg: string): boolean {
  const lower = msg.toLowerCase();
  return lower.includes("notfound") || lower.includes("not found") || msg.includes("笔记不存在");
}

export const NoteEditor = forwardRef<NoteEditorHandle, Props>(function NoteEditor(
  { noteKey, mode, onModeChange, onSaved, onDeleted, onDirtyChange, onNavigate, onCreate, onRenamed, onOpenSettings, refreshToken, previewContainerRef: externalPreviewRef },
  ref,
) {
  const [note, setNote] = useState<NoteDto | null>(null);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [renameOpen, setRenameOpen] = useState(false);
  const [linkDialogOpen, setLinkDialogOpen] = useState(false);

  const previewScrollRef = useRef<HTMLDivElement>(null);
  const previewProgressRef = useRef<HTMLDivElement>(null);
  const rafPreview = useRef<number | null>(null);
  const rafEdit = useRef<number | null>(null);
  const cmRef = useRef<MarkdownEditorHandle>(null);
  const editorContentRef = useRef<HTMLDivElement>(null);

  // capture body at mount for MarkdownEditor initialDoc — component is keyed by selectedKey so this is the note's body at open
  const bodyAtMountRef = useRef(body);
  // keep bodyAtMountRef in sync until the first note load completes
  const hasLoadedRef = useRef(false);
  useEffect(() => {
    if (!hasLoadedRef.current) bodyAtMountRef.current = body;
  }, [body]);

  // completion notes
  const notesRef = useRef<NoteSummary[]>([]);
  useEffect(() => {
    let cancelled = false;
    listNotes()
      .then((data) => {
        if (!cancelled) notesRef.current = data;
      })
      .catch(() => {
        if (!cancelled) notesRef.current = [];
      });
    return () => {
      cancelled = true;
    };
  }, [refreshToken]);
  const getCompletionNotes = useCallback(() => notesRef.current as unknown as { key: string; title: string; tags?: string[]; modified?: number }[], []);

  useEffect(() => {
    if (!noteKey) {
      setNote(null);
      setTitle("");
      setBody("");
      hasLoadedRef.current = false;
      bodyAtMountRef.current = "";
      setError(null);
      onDirtyChange(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    hasLoadedRef.current = false;
    getNote(noteKey)
      .then((data) => {
        if (cancelled) return;
        setNote(data);
        setTitle(data.title);
        setBody(data.body);
        bodyAtMountRef.current = data.body;
        hasLoadedRef.current = true;
        // sync CM doc after async load — MarkdownEditor mounted with "" before load
        const view = cmRef.current?.view();
        if (view && view.state.doc.toString() !== data.body) {
          view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: data.body } });
        }
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setNote(null);
        hasLoadedRef.current = true;
        setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [noteKey, onDirtyChange]);

  const dirty = note ? title !== note.title || body !== note.body : false;

  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);

  // ---------- autosave machinery ----------
  const autosaveTimerRef = useRef<number | null>(null);
  const savingRef = useRef(false);
  const pendingSaveRef = useRef(false);
  const inFlightPromiseRef = useRef<Promise<void> | null>(null);
  const lastSaveErrorRef = useRef<unknown | null>(null);

  const noteRef = useRef<NoteDto | null>(null);
  useEffect(() => {
    noteRef.current = note;
  }, [note]);
  const titleRef = useRef(title);
  useEffect(() => {
    titleRef.current = title;
  }, [title]);
  const bodyRef = useRef(body);
  useEffect(() => {
    bodyRef.current = body;
  }, [body]);
  const noteKeyRef = useRef(noteKey);
  useEffect(() => {
    noteKeyRef.current = noteKey;
  }, [noteKey]);
  const loadingRef = useRef(loading);
  useEffect(() => {
    loadingRef.current = loading;
  }, [loading]);
  const errorRef = useRef<string | null>(null);
  useEffect(() => {
    errorRef.current = error;
  }, [error]);
  const onSavedRef = useRef(onSaved);
  useEffect(() => {
    onSavedRef.current = onSaved;
  }, [onSaved]);

  // clear timer on unmount
  useEffect(() => {
    return () => {
      if (autosaveTimerRef.current !== null) {
        window.clearTimeout(autosaveTimerRef.current);
        autosaveTimerRef.current = null;
      }
    };
  }, []);
  // clear timer on noteKey change
  useEffect(() => {
    if (autosaveTimerRef.current !== null) {
      window.clearTimeout(autosaveTimerRef.current);
      autosaveTimerRef.current = null;
    }
    pendingSaveRef.current = false;
    lastSaveErrorRef.current = null;
  }, [noteKey]);

  const computeDirtyNow = useCallback(() => {
    const n = noteRef.current;
    if (!n) return false;
    const v = cmRef.current?.view();
    const curBody = v ? v.state.doc.toString() : bodyRef.current;
    const curTitle = titleRef.current;
    return curTitle !== n.title || curBody !== n.body;
  }, []);

  const executeSave = useCallback(async () => {
    const nk = noteKeyRef.current;
    const n = noteRef.current;
    if (!nk || !n) return;
    const v = cmRef.current?.view();
    const curBody = v ? v.state.doc.toString() : bodyRef.current;
    const curTitle = titleRef.current;
    if (curTitle === n.title && curBody === n.body) return;
    const sentBody = curBody;
    const sentTitle = curTitle;
    const sentKey = nk;

    savingRef.current = true;
    setSaving(true);
    setError(null);
    lastSaveErrorRef.current = null;

    const p = (async () => {
      const updated = await saveNote(sentKey, sentBody, sentTitle.trim() || undefined);
      if (noteKeyRef.current !== sentKey) return;
      const nowView = cmRef.current?.view();
      const nowBody = nowView ? nowView.state.doc.toString() : bodyRef.current;
      const nowTitle = titleRef.current;
      const bodyChanged = nowBody !== sentBody;
      const titleChanged = nowTitle !== sentTitle;
      if (!bodyChanged && !titleChanged) {
        setNote(updated);
        noteRef.current = updated;
        setTitle(updated.title);
        titleRef.current = updated.title;
        setBody(updated.body);
        bodyRef.current = updated.body;
        if (nowView && nowView.state.doc.toString() !== updated.body) {
          nowView.dispatch({ changes: { from: 0, to: nowView.state.doc.length, insert: updated.body } });
        }
      } else {
        setNote(updated);
        noteRef.current = updated;
      }
      onSavedRef.current();
    })();

    inFlightPromiseRef.current = p;
    try {
      await p;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
      lastSaveErrorRef.current = e;
      throw e;
    } finally {
      inFlightPromiseRef.current = null;
      setSaving(false);
      savingRef.current = false;
    }
  }, []);

  const runSaveAfterPending = useCallback(async () => {
    try {
      await executeSave();
      while (pendingSaveRef.current) {
        pendingSaveRef.current = false;
        if (!noteKeyRef.current) break;
        if (loadingRef.current) break;
        if (errorRef.current) break;
        if (!computeDirtyNow()) break;
        await executeSave();
      }
    } catch {
      pendingSaveRef.current = false;
      throw lastSaveErrorRef.current ?? new Error("保存失败");
    }
  }, [computeDirtyNow, executeSave]);

  // autosave debounce effect
  useEffect(() => {
    if (!noteKey) return;
    if (!dirty) return;
    if (loading) return;
    if (error) return;
    if (autosaveTimerRef.current !== null) {
      window.clearTimeout(autosaveTimerRef.current);
      autosaveTimerRef.current = null;
    }
    autosaveTimerRef.current = window.setTimeout(() => {
      autosaveTimerRef.current = null;
      if (!noteKeyRef.current) return;
      if (loadingRef.current) return;
      if (errorRef.current) return;
      if (!computeDirtyNow()) return;
      if (savingRef.current) {
        pendingSaveRef.current = true;
        return;
      }
      void runSaveAfterPending().catch(() => {
        // error already surfaced via setError
      });
    }, 1500);
    return () => {
      if (autosaveTimerRef.current !== null) {
        window.clearTimeout(autosaveTimerRef.current);
        autosaveTimerRef.current = null;
      }
    };
  }, [noteKey, dirty, loading, error, computeDirtyNow, runSaveAfterPending]);

  const flushSave = useCallback(async () => {
    if (autosaveTimerRef.current !== null) {
      window.clearTimeout(autosaveTimerRef.current);
      autosaveTimerRef.current = null;
    }
    if (savingRef.current && inFlightPromiseRef.current) {
      try {
        await inFlightPromiseRef.current;
      } catch {
        // lastSaveErrorRef already set; continue to check dirty
      }
    }
    const n = noteRef.current;
    if (!n || !noteKeyRef.current) {
      if (lastSaveErrorRef.current) {
        const e = lastSaveErrorRef.current;
        lastSaveErrorRef.current = null;
        throw e;
      }
      return;
    }
    if (loadingRef.current) {
      if (lastSaveErrorRef.current) {
        const e = lastSaveErrorRef.current;
        throw e;
      }
      return;
    }
    if (!computeDirtyNow()) {
      if (lastSaveErrorRef.current) {
        const e = lastSaveErrorRef.current;
        lastSaveErrorRef.current = null;
        throw e;
      }
      return;
    }
    lastSaveErrorRef.current = null;
    pendingSaveRef.current = false;
    await executeSave();
    while (pendingSaveRef.current) {
      pendingSaveRef.current = false;
      if (!noteKeyRef.current || loadingRef.current || errorRef.current) break;
      if (!computeDirtyNow()) break;
      await executeSave();
    }
    let extraAttempts = 0;
    while (computeDirtyNow() && extraAttempts < 2) {
      if (!noteKeyRef.current || loadingRef.current || errorRef.current) break;
      await executeSave();
      extraAttempts++;
      while (pendingSaveRef.current) {
        pendingSaveRef.current = false;
        if (!noteKeyRef.current || loadingRef.current || errorRef.current) break;
        if (!computeDirtyNow()) break;
        await executeSave();
      }
    }
    if (computeDirtyNow() && lastSaveErrorRef.current) {
      const e = lastSaveErrorRef.current;
      lastSaveErrorRef.current = null;
      throw e;
    }
  }, [computeDirtyNow, executeSave]);

  // 预览进度条：直接改 DOM 宽度，避免 rerender
  const syncPreviewProgress = useCallback(() => {
    const el = previewScrollRef.current;
    const bar = previewProgressRef.current;
    if (!el || !bar) return;
    const max = el.scrollHeight - el.clientHeight;
    if (max <= 0) {
      bar.style.width = "0%";
      bar.style.opacity = "0";
      return;
    }
    bar.style.opacity = "1";
    const pct = (el.scrollTop / max) * 100;
    bar.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  }, []);

  // Ctrl/Cmd+E 切换模式
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (!noteKey) return;
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "e") {
        e.preventDefault();
        // 保存当前侧滚动到记忆
        const k = noteKeyRef.current ?? "";
        if (mode === "edit") {
          const v = cmRef.current?.view();
          if (v) writeScrollPos(k, "edit", v.scrollDOM.scrollTop);
        } else if (previewScrollRef.current) {
          writeScrollPos(k, "preview", previewScrollRef.current.scrollTop);
        }
        onModeChange(mode === "edit" ? "preview" : "edit");
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [noteKey, mode, onModeChange]);

  // 模式切换时恢复滚动位置 — rAF deferred so CM has laid out
  useEffect(() => {
    const k = noteKeyRef.current ?? "";
    const pos = readScrollPos(k);
    const id = requestAnimationFrame(() => {
      if (mode === "edit") {
        const view = cmRef.current?.view();
        if (view) view.scrollDOM.scrollTop = pos.edit;
      } else if (previewScrollRef.current) {
        previewScrollRef.current.scrollTop = pos.preview;
        // 同步进度条到恢复后位置
        syncPreviewProgress();
      }
    });
    return () => cancelAnimationFrame(id);
  }, [mode, syncPreviewProgress]);

  // 编辑态：监听 CM scrollDOM（rAF 节流）。view 可能在 effect 首次执行时尚未创建，用 rAF 重试一次。
  useEffect(() => {
    if (mode !== "edit") return;
    let cleanup: (() => void) | null = null;
    let rafAttach: number | null = null;
    const attach = () => {
      const view = cmRef.current?.view();
      if (!view) {
        rafAttach = requestAnimationFrame(attach);
        return;
      }
      const el = view.scrollDOM;
      const onScroll = () => {
        if (rafEdit.current !== null) return;
        rafEdit.current = requestAnimationFrame(() => {
          rafEdit.current = null;
          writeScrollPos(noteKeyRef.current ?? "", "edit", el.scrollTop);
        });
      };
      el.addEventListener("scroll", onScroll);
      cleanup = () => el.removeEventListener("scroll", onScroll);
    };
    attach();
    return () => {
      if (rafAttach !== null) cancelAnimationFrame(rafAttach);
      cleanup?.();
    };
  }, [mode, noteKey, loading]);

  const handlePreviewScroll = useCallback(() => {
    if (rafPreview.current !== null) return;
    rafPreview.current = requestAnimationFrame(() => {
      rafPreview.current = null;
      const el = previewScrollRef.current;
      if (el) {
        writeScrollPos(noteKeyRef.current ?? "", "preview", el.scrollTop);
        syncPreviewProgress();
      }
    });
  }, [syncPreviewProgress]);

  // 预览内容变化后校准进度条（首帧与内容增高时）
  useEffect(() => {
    if (mode !== "preview") return;
    const id = requestAnimationFrame(() => syncPreviewProgress());
    return () => cancelAnimationFrame(id);
  }, [mode, body, syncPreviewProgress]);

  const handleDelete = async () => {
    if (!noteKey) return;
    const ok = window.confirm(`确定删除「${note?.title ?? noteKey}」吗？此操作不可撤销。`);
    if (!ok) return;
    setError(null);
    try {
      await deleteNote(noteKey);
      onDeleted(noteKey);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const handleRename = useCallback(
    async (raw: string) => {
      if (!noteKey || !note) return;
      const normalized = normalizeNoteKey(raw);
      if (normalized === noteKey) return;
      const err = validateNoteKey(raw);
      if (err) throw new Error(err);
      try {
        await getNote(normalized);
        throw new Error("已存在同名笔记");
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        if (!isNotFoundError(msg) && msg !== "已存在同名笔记") {
          throw e;
        }
        if (msg === "已存在同名笔记") throw e;
      }
      try {
        const renamed = await renameNote(noteKey, normalized, true);
        setNote(renamed);
        noteRef.current = renamed;
        setTitle(renamed.title);
        titleRef.current = renamed.title;
        setBody(renamed.body);
        bodyRef.current = renamed.body;
        const view = cmRef.current?.view();
        if (view && view.state.doc.toString() !== renamed.body) {
          view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: renamed.body } });
        }
        setRenameOpen(false);
        onRenamed?.(noteKey, normalized);
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        window.alert(msg);
        throw e;
      }
    },
    [noteKey, note, onRenamed],
  );

  // 外部预览容器 ref 同步（用于 TocPanel 预览态跳转）
  useEffect(() => {
    if (!externalPreviewRef) return;
    const el = previewScrollRef.current;
    if (el) {
      (externalPreviewRef as React.MutableRefObject<HTMLDivElement | null>).current = el;
    }
  }, [mode, externalPreviewRef, noteKey]);

  // selection source adapter over CM view — stable via useMemo on cmRef/editorContentRef identity
  const selectionSource: SelectionSource = useMemo(() => {
    const getView = () => cmRef.current?.view() ?? null;
    return {
      getSelection() {
        const view = getView();
        if (!view) return null;
        const sel = view.state.selection.main;
        if (sel.from === sel.to) return null;
        const text = view.state.sliceDoc(sel.from, sel.to);
        if (!text.trim()) return null;
        return { text, start: sel.from, end: sel.to };
      },
      getCaretRect(pos: number) {
        const view = getView();
        if (!view) return null;
        const container = editorContentRef.current;
        if (!container) return null;
        const coords = view.coordsAtPos(pos);
        if (!coords) return null;
        const cRect = container.getBoundingClientRect();
        return { top: coords.top - cRect.top, left: coords.left - cRect.left };
      },
      replaceRange(from: number, to: number, text: string) {
        const view = getView();
        if (!view) return;
        view.dispatch({ changes: { from, to, insert: text }, selection: { anchor: from + text.length } });
        view.focus();
      },
      insertAt(pos: number, text: string) {
        const view = getView();
        if (!view) return;
        view.dispatch({ changes: { from: pos, insert: text }, selection: { anchor: pos + text.length } });
        view.focus();
      },
      focus() {
        getView()?.focus();
      },
    };
  }, []);

  // 底栏统计：字符/词/行（由 body 派生）
  const bodyStats = useMemo(() => {
    const chars = [...body].length;
    const trimmed = body.trim();
    const words = trimmed === "" ? 0 : trimmed.split(/\s+/).length;
    const lines = body === "" ? 0 : body.split("\n").length;
    return { chars, words, lines };
  }, [body]);

  const isBodyEmpty = body.trim() === "";

  // —— 命令句柄 ——
  useImperativeHandle(
    ref,
    () => ({
      getBody() {
        const view = cmRef.current?.view();
        if (view) return view.state.doc.toString();
        return body;
      },
      getTitle() {
        return title;
      },
      getEditorView() {
        return cmRef.current?.view() ?? null;
      },
      insertAtCursor(text: string) {
        const view = cmRef.current?.view();
        if (!view) return;
        if (mode === "preview") onModeChange("edit");
        view.dispatch(view.state.replaceSelection(text));
        view.focus();
      },
      replaceSelection(text: string) {
        const view = cmRef.current?.view();
        if (!view) return;
        if (mode === "preview") onModeChange("edit");
        view.dispatch(view.state.replaceSelection(text));
        view.focus();
      },
      getSelection() {
        const view = cmRef.current?.view();
        if (!view) return null;
        const sel = view.state.selection.main;
        if (sel.from === sel.to) return null;
        const text = view.state.sliceDoc(sel.from, sel.to);
        if (!text) return null;
        return { text, start: sel.from, end: sel.to };
      },
      scrollToLine(line: number) {
        const view = cmRef.current?.view();
        if (!view) return;
        if (mode === "preview") onModeChange("edit");
        const l = Math.min(line + 1, view.state.doc.lines);
        const pos = view.state.doc.line(l).from;
        view.dispatch({ selection: { anchor: pos }, effects: EditorView.scrollIntoView(pos, { y: "start" }) });
        view.focus();
      },
      appendToBody(text: string) {
        const view = cmRef.current?.view();
        if (!view) {
          setBody((prev) => (prev ? prev + text : text));
          return;
        }
        if (mode === "preview") onModeChange("edit");
        const end = view.state.doc.length;
        view.dispatch({ changes: { from: end, insert: text }, selection: { anchor: end + text.length } });
        view.focus();
      },
      flushSave,
    }),
    [body, title, mode, onModeChange, flushSave],
  );

  if (!noteKey) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-center">
        <FileText className="size-10 text-muted-foreground/50" />
        <p className="text-sm font-medium">未选中笔记</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          从左侧列表选择一篇笔记开始阅读，或切换到编辑模式进行修改。
        </p>
      </div>
    );
  }

  if (loading) {
    return <div className="p-6 text-sm text-muted-foreground">加载中…</div>;
  }

  if (error && !note) {
    if (isNotFoundError(error) && noteKey) {
      return (
        <div className="flex h-full flex-col items-center justify-center gap-4 p-8 text-center">
          <p className="text-sm text-muted-foreground">笔记不存在</p>
          <p className="text-xs text-muted-foreground">
            <code className="rounded bg-muted px-1.5 py-0.5">{noteKey}</code>
          </p>
          {onCreate ? (
            <Button onClick={() => onCreate(noteKey)}>创建该笔记</Button>
          ) : (
            <p className="text-sm text-destructive">{error}</p>
          )}
        </div>
      );
    }
    return <div className="p-6 text-sm text-destructive">{error}</div>;
  }

  const isEdit = mode !== "preview";

  return (
    <div className="flex h-full flex-col">
      {/* 工具栏 */}
      <div className="flex items-center gap-1 border-b border-border/60 px-3 py-1.5">
        <div className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
          {note && (
            <>
              <code className="rounded bg-muted px-1 py-0.5">{note.key}</code>
              {note.aliases.length > 0 && <span className="ml-2">别名: {note.aliases.join(", ")}</span>}
              {note.tags.length > 0 && <span className="ml-2">标签: {note.tags.join(", ")}</span>}
            </>
          )}
        </div>
        {saving ? (
          <span className="mr-2 shrink-0 text-xs text-muted-foreground">保存中…</span>
        ) : dirty ? (
          <span className="mr-2 shrink-0 text-xs text-amber-600">有未保存的改动</span>
        ) : null}
        {isEdit && (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-8 shrink-0"
            onClick={() => setLinkDialogOpen(true)}
            title="AI 建议"
            aria-label="AI 建议"
          >
            <Sparkles className="size-4" />
          </Button>
        )}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          onClick={() => onModeChange(isEdit ? "preview" : "edit")}
          title={isEdit ? "预览 (Ctrl+E)" : "编辑 (Ctrl+E)"}
          aria-label={isEdit ? "预览" : "编辑"}
        >
          {isEdit ? <Eye className="size-4" /> : <Pencil className="size-4" />}
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          onClick={() => void flushSave()}
          disabled={saving || !dirty}
          title="保存"
          aria-label="保存"
        >
          <Save className="size-4" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          onClick={() => setRenameOpen(true)}
          title="重命名"
          aria-label="重命名"
        >
          <FilePenLine className="size-4" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8 shrink-0 hover:bg-destructive/10 hover:text-destructive"
          onClick={handleDelete}
          title="删除"
          aria-label="删除"
        >
          <Trash2 className="size-4" />
        </Button>
      </div>
      {isEdit && <EditorToolbar getView={() => cmRef.current?.view() ?? null} />}

      {error && <p className="mx-3 mt-3 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>}

      {/* 编辑区：preview 模式下 hidden 但保持挂载，保留撤销历史与选区 */}
      <div className={`flex min-h-0 flex-1 flex-col ${isEdit ? "" : "hidden"}`}>
        <div className="px-4 pt-3">
          <div className="mx-auto w-full max-w-[760px]">
            <Input
              id="note-title"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder="笔记标题"
              className="border-0 bg-transparent px-0 text-xl font-semibold shadow-none focus-visible:ring-0"
            />
          </div>
        </div>
        <div ref={editorContentRef} className="relative mt-3 flex min-h-0 flex-1 flex-col px-4 pb-3">
          <div className="mx-auto flex min-h-0 w-full max-w-[760px] flex-1 flex-col">
            <MarkdownEditor
              ref={cmRef}
              initialDoc={bodyAtMountRef.current}
              onDocChanged={setBody}
              onSave={() => void flushSave()}
              getCompletionNotes={getCompletionNotes}
              onPasteImage={async (file) => {
                if (!noteKey) return null;
                try {
                  const bytes = new Uint8Array(await file.arrayBuffer());
                  const name = file.name || "pasted.png";
                  const { rel_path } = await saveAttachment(noteKey, name, bytes);
                  return `/${rel_path}`;
                } catch (e: unknown) {
                  const msg = e instanceof Error ? e.message : String(e);
                  setError(msg || "图片保存失败");
                  return null;
                }
              }}
              onImageError={(msg) => setError(msg)}
              placeholder="在此输入正文…（Markdown，支持 [[wikilink]]）"
              className="min-h-0 flex-1"
            />
          </div>
          {isEdit && (
            <SelectionToolbar
              source={selectionSource}
              containerRef={editorContentRef}
              noteTitle={note?.title ?? title ?? noteKey}
              noteBody={body}
              noteKey={noteKey}
              onOpenSettings={() => onOpenSettings?.()}
            />
          )}
        </div>
      </div>

      {/* 预览区：edit 模式下 hidden — 原 overflow-auto 改为隐藏原生条 + 自绘悬浮条 */}
      <div className={`relative flex min-h-0 flex-1 flex-col ${isEdit ? "hidden" : ""}`}>
        {/* 阅读进度条：零 rerender，直接改 width */}
        <div className="pointer-events-none absolute left-0 right-0 top-0 z-10 h-0.5 overflow-hidden">
          <div
            ref={previewProgressRef}
            className="h-full bg-primary transition-[width] duration-75"
            style={{ width: "0%", opacity: 0 }}
          />
        </div>
        <div
          ref={previewScrollRef}
          onScroll={handlePreviewScroll}
          className="no-native-scrollbar flex min-h-0 flex-1 flex-col overflow-auto px-4 py-3"
        >
          {isBodyEmpty ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-2 py-16 text-center">
              <FileText className="size-8 text-muted-foreground/40" />
              <p className="text-sm text-muted-foreground">这篇笔记还没有内容</p>
              <p className="text-xs text-muted-foreground">按 Ctrl+E 开始编辑</p>
            </div>
          ) : (
            <div
              key={`${noteKey}-${mode}`}
              className="mx-auto w-full max-w-[760px] animate-in fade-in duration-150"
            >
              <h1 className="text-xl font-semibold">{title || note?.title || noteKey}</h1>
              {note && (note.aliases.length > 0 || note.tags.length > 0) && (
                <p className="mt-1 text-xs text-muted-foreground">
                  {note.aliases.length > 0 && <>别名: {note.aliases.join(", ")} </>}
                  {note.tags.length > 0 && <>标签: {note.tags.join(", ")}</>}
                </p>
              )}
              <MarkdownPreview content={body} onNavigate={onNavigate} />
            </div>
          )}
        </div>
        <OverlayScrollbar containerRef={previewScrollRef} />
      </div>

      {/* 底部状态栏：仅在正常笔记视图展示 */}
      <div className="flex h-7 shrink-0 items-center gap-3 border-t border-border/60 px-3 text-xs text-muted-foreground">
        <span className="shrink-0">{isEdit ? "编辑" : "预览"}</span>
        <span className="shrink-0">
          {bodyStats.chars} 字符 · {bodyStats.words} 词 · {bodyStats.lines} 行
        </span>
        <span className="min-w-0 flex-1 truncate text-center">
          {noteKey ? <code className="rounded bg-muted px-1 py-0.5">{noteKey}</code> : null}
        </span>
        <span className="shrink-0">
          {saving ? "保存中…" : dirty ? "有未保存的改动" : "已保存"}
        </span>
      </div>

      {renameOpen && noteKey && (
        <NoteFormDialog
          title="重命名笔记"
          label="新标题"
          initial={noteKey}
          placeholder="输入新的 key，例如 folder/note"
          hint="重命名不会自动更新其他笔记中的 [[链接]]"
          submitText="重命名"
          validate={validateNoteKey}
          onSubmit={handleRename}
          onClose={() => setRenameOpen(false)}
        />
      )}

      {linkDialogOpen && (
        <LinkSuggestDialog
          noteTitle={note?.title ?? title ?? noteKey}
          noteBody={body}
          onClose={() => setLinkDialogOpen(false)}
          onInsertRef={(text) => {
            const view = cmRef.current?.view();
            if (!view) return;
            if (mode === "preview") onModeChange("edit");
            view.dispatch(view.state.replaceSelection(text));
            view.focus();
          }}
          onAppendTag={(tagText) => {
            const suffix = body.endsWith("\n") || body.length === 0 ? tagText : ` ${tagText}`;
            const view = cmRef.current?.view();
            if (!view) {
              setBody((prev) => prev + suffix);
              return;
            }
            if (mode === "preview") onModeChange("edit");
            const end = view.state.doc.length;
            view.dispatch({ changes: { from: end, insert: suffix }, selection: { anchor: end + suffix.length } });
            view.focus();
          }}
          onOpenSettings={() => {
            setLinkDialogOpen(false);
            onOpenSettings?.();
          }}
        />
      )}
    </div>
  );
});
