import { useCallback, useEffect, useImperativeHandle, useRef, useState, forwardRef, useMemo } from "react";
import { Save, Trash2, FileText, FilePenLine, Sparkles } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { getNote, saveNote, deleteNote, renameNote, listNotes, saveAttachment } from "@/api/tauri";
import type { NoteDto, NoteSummary } from "@/api/types";
import { NoteFormDialog } from "@/components/NoteFormDialog";
import { normalizeNoteKey, validateNoteKey } from "@/lib/note-key";
import { SelectionToolbar } from "@/components/ai/SelectionToolbar";
import { LinkSuggestDialog } from "@/components/ai/LinkSuggestDialog";
import { MarkdownEditor, type MarkdownEditorHandle } from "@/components/editor/MarkdownEditor";
import { EditorToolbar } from "@/components/editor/EditorToolbar";
import { EditorView } from "@codemirror/view";
import type { SelectionSource } from "@/lib/selection-source";
import { readScrollPos, writeScrollPos } from "@/lib/scroll-memory";
import { loadEditorPrefs, saveEditorPrefs } from "@/lib/editor-prefs";

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
  onSaved: () => void;
  onDeleted: (deletedKey?: string) => void;
  onDirtyChange: (dirty: boolean) => void;
  onNavigate?: (key: string) => void;
  onCreate?: (key: string) => void;
  onRenamed?: (oldKey: string, newKey: string) => void;
  onOpenSettings?: () => void;
  refreshToken?: number;
};

function isNotFoundError(msg: string): boolean {
  const lower = msg.toLowerCase();
  return lower.includes("notfound") || lower.includes("not found") || msg.includes("笔记不存在");
}

/**
 * 差异替换文档并保留选区：计算旧文档与新文本的公共前/后缀，
 * 只 dispatch 中间差异区间（selection 经 CM change mapping 自然保留）；
 * 文档相同时不 dispatch，避免视图重建与光标/焦点/撤销历史丢失。
 */
function replaceDocPreserveSelection(view: EditorView, text: string): void {
  const old = view.state.doc.toString();
  if (old === text) return;
  let prefix = 0;
  const minLen = Math.min(old.length, text.length);
  while (prefix < minLen && old.charCodeAt(prefix) === text.charCodeAt(prefix)) prefix++;
  let suffix = 0;
  while (
    suffix < minLen - prefix &&
    old.charCodeAt(old.length - 1 - suffix) === text.charCodeAt(text.length - 1 - suffix)
  ) {
    suffix++;
  }
  view.dispatch({
    changes: { from: prefix, to: old.length - suffix, insert: text.slice(prefix, text.length - suffix) },
  });
}

export const NoteEditor = forwardRef<NoteEditorHandle, Props>(function NoteEditor(
  { noteKey, onSaved, onDeleted, onDirtyChange, onNavigate, onCreate, onRenamed, onOpenSettings, refreshToken },
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
  // Live Preview 开关：默认开启，切换时写 localStorage
  const [livePreview, setLivePreview] = useState<boolean>(() => loadEditorPrefs().livePreview);
  const toggleLivePreview = useCallback(() => {
    setLivePreview((prev) => {
      const next = !prev;
      saveEditorPrefs({ livePreview: next });
      return next;
    });
  }, []);

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

  // 外部刷新（refreshToken 自增）时的脏保护：若当前笔记 dirty 且磁盘已被外部（agent）修改，弹确认框
  const doReloadNote = useCallback(
    (targetKey: string) => {
      let cancelled = false;
      // 仅初始加载（尚未持有笔记）时展示 loading；后台 reload 不再触发 loading，
      // 编辑器保持挂载，避免 CM 视图卸载重建导致光标/焦点/撤销历史丢失
      if (noteRef.current == null) setLoading(true);
      setError(null);
      hasLoadedRef.current = false;
      getNote(targetKey)
        .then((data) => {
          if (cancelled) return;
          setNote(data);
          setTitle(data.title);
          setBody(data.body);
          bodyAtMountRef.current = data.body;
          hasLoadedRef.current = true;
          const view = cmRef.current?.view();
          if (view) replaceDocPreserveSelection(view, data.body);
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
    },
    [],
  );

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
    // 首次加载（noteKey 变化）直接重载
    return doReloadNote(noteKey);
  }, [noteKey, onDirtyChange, doReloadNote]);

  // refreshToken 驱动的外部重载：带脏保护
  useEffect(() => {
    if (!noteKey) return;
    if (refreshToken == null) return;
    // 跳过首次 mount（noteKey 变化已触发 doReloadNote，且 refreshToken 初值为 0）
    // 用 hasLoadedRef 判断是否已完成首次加载；未完成则不处理 refreshToken
    if (!hasLoadedRef.current) return;
    // 判断是否 dirty（用 ref 避免闭包 stale）
    const isDirty = (() => {
      const n = noteRef.current;
      if (!n) return false;
      const v = cmRef.current?.view();
      const curBody = v ? v.state.doc.toString() : bodyRef.current;
      const curTitle = titleRef.current;
      return curTitle !== n.title || curBody !== n.body;
    })();
    if (!isDirty) {
      // 非 dirty：先探测磁盘是否变化，未变化则完全跳过
      // （不调 doReloadNote、不 setState），避免自动保存后的 refresh 打断编辑；
      // 磁盘确实变化时直接应用新内容
      let cancelled = false;
      getNote(noteKey)
        .then((remote) => {
          if (cancelled) return;
          const n = noteRef.current;
          if (!n) return;
          if (remote.body === n.body && remote.title === n.title) return;
          setNote(remote);
          setTitle(remote.title);
          setBody(remote.body);
          bodyAtMountRef.current = remote.body;
          hasLoadedRef.current = true;
          const view = cmRef.current?.view();
          if (view) replaceDocPreserveSelection(view, remote.body);
        })
        .catch(() => {
          // 探测失败不阻塞
        });
      return () => {
        cancelled = true;
      };
    }
    // dirty：先探测磁盘是否已被外部修改
    let cancelled = false;
    getNote(noteKey)
      .then((remote) => {
        if (cancelled) return;
        const n = noteRef.current;
        if (!n) return;
        const changedOnDisk = remote.body !== n.body || remote.title !== n.title;
        if (!changedOnDisk) return;
        const ok = window.confirm("磁盘已被 Agent 修改，重新加载？未保存的改动将丢失。");
        if (!ok) return;
        if (cancelled) return;
        setNote(remote);
        setTitle(remote.title);
        setBody(remote.body);
        bodyAtMountRef.current = remote.body;
        hasLoadedRef.current = true;
        const view = cmRef.current?.view();
        if (view) replaceDocPreserveSelection(view, remote.body);
      })
      .catch(() => {
        // 探测失败不阻塞
      });
    return () => {
      cancelled = true;
    };
  }, [refreshToken, noteKey, doReloadNote]);

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
        // 服务端可能归一化内容：差异替换，保留光标/选区
        if (nowView) replaceDocPreserveSelection(nowView, updated.body);
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

  // 笔记切换时恢复编辑侧滚动位置 — rAF deferred so CM has laid out
  useEffect(() => {
    const k = noteKeyRef.current ?? "";
    const pos = readScrollPos(k);
    const id = requestAnimationFrame(() => {
      const view = cmRef.current?.view();
      if (view) view.scrollDOM.scrollTop = pos;
    });
    return () => cancelAnimationFrame(id);
  }, [noteKey]);

  // 编辑区：监听 CM scrollDOM（rAF 节流）。view 可能在 effect 首次执行时尚未创建，用 rAF 重试一次。
  useEffect(() => {
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
          writeScrollPos(noteKeyRef.current ?? "", el.scrollTop);
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
  }, [noteKey, loading]);

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
        if (view) replaceDocPreserveSelection(view, renamed.body);
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
        view.dispatch(view.state.replaceSelection(text));
        view.focus();
      },
      replaceSelection(text: string) {
        const view = cmRef.current?.view();
        if (!view) return;
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
        const end = view.state.doc.length;
        view.dispatch({ changes: { from: end, insert: text }, selection: { anchor: end + text.length } });
        view.focus();
      },
      flushSave,
    }),
    [body, title, flushSave],
  );

  if (!noteKey) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-center">
        <FileText className="size-10 text-muted-foreground/50" />
        <p className="text-sm font-medium">未选中笔记</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          从左侧列表选择一篇笔记开始编辑。
        </p>
      </div>
    );
  }

  // 仅初始加载展示 loading；后台 reload 保持编辑器挂载，避免打断编辑
  if (loading && note == null) {
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
      <EditorToolbar
        getView={() => cmRef.current?.view() ?? null}
        livePreview={livePreview}
        onToggleLivePreview={toggleLivePreview}
      />

      {error && <p className="mx-3 mt-3 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>}

      {/* 编辑区：恒为编辑态；空正文时编辑器照常渲染（CM placeholder 已有提示） */}
      <div className="flex min-h-0 flex-1 flex-col">
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
              livePreviewEnabled={livePreview}
              onNavigateWikilink={onNavigate}
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
          <SelectionToolbar
            source={selectionSource}
            containerRef={editorContentRef}
            noteTitle={note?.title ?? title ?? noteKey}
            noteBody={body}
            noteKey={noteKey}
            onOpenSettings={() => onOpenSettings?.()}
          />
        </div>
      </div>

      {/* 底部状态栏：仅在正常笔记视图展示 */}
      <div className="flex h-7 shrink-0 items-center gap-3 border-t border-border/60 px-3 text-xs text-muted-foreground">
        <span className="shrink-0">编辑</span>
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
