import { useCallback, useEffect, useRef, useState } from "react";
import { Eye, Pencil, Save, Trash2, FileText, FilePenLine } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { getNote, saveNote, deleteNote } from "@/api/tauri";
import type { NoteDto } from "@/api/types";
import { MarkdownPreview } from "@/components/MarkdownPreview";
import { NoteFormDialog } from "@/components/NoteFormDialog";
import { normalizeNoteKey, validateNoteKey } from "@/lib/note-key";

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
};

function isNotFoundError(msg: string): boolean {
  const lower = msg.toLowerCase();
  return lower.includes("notfound") || lower.includes("not found") || msg.includes("笔记不存在");
}

export function NoteEditor({
  noteKey,
  mode,
  onModeChange,
  onSaved,
  onDeleted,
  onDirtyChange,
  onNavigate,
  onCreate,
  onRenamed,
}: Props) {
  const [note, setNote] = useState<NoteDto | null>(null);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [renameOpen, setRenameOpen] = useState(false);

  const scrollPos = useRef({ edit: 0, preview: 0 });
  const editScrollRef = useRef<HTMLDivElement>(null);
  const previewScrollRef = useRef<HTMLDivElement>(null);
  const rafEdit = useRef<number | null>(null);
  const rafPreview = useRef<number | null>(null);

  useEffect(() => {
    if (!noteKey) {
      setNote(null);
      setTitle("");
      setBody("");
      setError(null);
      onDirtyChange(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    getNote(noteKey)
      .then((data) => {
        if (cancelled) return;
        setNote(data);
        setTitle(data.title);
        setBody(data.body);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setNote(null);
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

  // Ctrl/Cmd+E 切换模式
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (!noteKey) return;
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "e") {
        e.preventDefault();
        onModeChange(mode === "edit" ? "preview" : "edit");
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [noteKey, mode, onModeChange]);

  // 模式切换时恢复滚动位置
  useEffect(() => {
    const id = requestAnimationFrame(() => {
      if (mode === "edit" && editScrollRef.current) {
        editScrollRef.current.scrollTop = scrollPos.current.edit;
      } else if (mode === "preview" && previewScrollRef.current) {
        previewScrollRef.current.scrollTop = scrollPos.current.preview;
      }
    });
    return () => cancelAnimationFrame(id);
  }, [mode]);

  const handleEditScroll = useCallback(() => {
    if (rafEdit.current !== null) return;
    rafEdit.current = requestAnimationFrame(() => {
      rafEdit.current = null;
      if (editScrollRef.current) scrollPos.current.edit = editScrollRef.current.scrollTop;
    });
  }, []);

  const handlePreviewScroll = useCallback(() => {
    if (rafPreview.current !== null) return;
    rafPreview.current = requestAnimationFrame(() => {
      rafPreview.current = null;
      if (previewScrollRef.current) scrollPos.current.preview = previewScrollRef.current.scrollTop;
    });
  }, []);

  const handleSave = async () => {
    if (!noteKey || !note) return;
    setSaving(true);
    setError(null);
    try {
      const updated = await saveNote(noteKey, body, title.trim() || undefined);
      setNote(updated);
      setTitle(updated.title);
      setBody(updated.body);
      onSaved();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

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
      // 查重：若 getNote 能取到则已存在
      try {
        await getNote(normalized);
        throw new Error("已存在同名笔记");
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        if (!isNotFoundError(msg) && msg !== "已存在同名笔记") {
          // 非"不存在"错误（网络/权限等）直接抛出
          throw e;
        }
        if (msg === "已存在同名笔记") throw e;
        // 不存在 → 可继续
      }
      const titleToSave = title.trim() || undefined;
      const saved = await saveNote(normalized, body, titleToSave);
      try {
        await deleteNote(noteKey);
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        window.alert(`重命名已保存新笔记，但删除旧笔记失败：${msg}`);
      }
      setNote(saved);
      setTitle(saved.title);
      setBody(saved.body);
      setRenameOpen(false);
      onRenamed?.(noteKey, normalized);
    },
    [noteKey, note, title, body, onRenamed],
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

  const isEdit = mode === "preview" ? false : true;

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
        {dirty && <span className="mr-2 shrink-0 text-xs text-amber-600">有未保存的改动</span>}
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
          onClick={handleSave}
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

      {error && <p className="mx-3 mt-3 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>}

      {isEdit ? (
        <div ref={editScrollRef} onScroll={handleEditScroll} className="flex min-h-0 flex-1 flex-col overflow-auto px-4 py-3">
          <Input
            id="note-title"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="笔记标题"
            className="border-0 bg-transparent px-0 text-xl font-semibold shadow-none focus-visible:ring-0"
          />
          <textarea
            id="note-body"
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder="在此输入正文…（Markdown，支持 [[wikilink]]）"
            className="mt-3 min-h-0 flex-1 resize-none bg-transparent font-mono text-sm placeholder:text-muted-foreground focus-visible:outline-none"
          />
        </div>
      ) : (
        <div ref={previewScrollRef} onScroll={handlePreviewScroll} className="flex min-h-0 flex-1 flex-col overflow-auto px-4 py-3">
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
    </div>
  );
}
