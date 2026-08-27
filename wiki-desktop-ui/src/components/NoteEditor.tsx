import { useEffect, useState } from "react";
import { Save, Trash2, FileText } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { getNote, saveNote, deleteNote } from "@/api/tauri";
import type { NoteDto } from "@/api/types";

type Props = {
  noteKey: string | null;
  onSaved: () => void;
  onDeleted: () => void;
  onDirtyChange: (dirty: boolean) => void;
};

export function NoteEditor({ noteKey, onSaved, onDeleted, onDirtyChange }: Props) {
  const [note, setNote] = useState<NoteDto | null>(null);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
        // 笔记不存在时后端返回 Err(NoteNotFound)，promise 在此 reject
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
      onDeleted();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (!noteKey) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-center">
        <FileText className="size-10 text-muted-foreground/50" />
        <p className="text-sm font-medium">未选中笔记</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          从左侧列表选择一篇笔记开始编辑，或在浏览器 dev 模式下通过 mock 数据体验完整流程。
        </p>
      </div>
    );
  }

  if (loading) {
    return <div className="p-6 text-sm text-muted-foreground">加载中…</div>;
  }

  if (error && !note) {
    return <div className="p-6 text-sm text-destructive">{error}</div>;
  }

  return (
    <div className="flex h-full flex-col gap-4 p-4">
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center justify-between gap-2 text-base">
            <span>编辑笔记</span>
            {dirty && <span className="text-xs font-normal text-amber-600">有未保存的改动</span>}
          </CardTitle>
          {note && (
            <p className="text-xs text-muted-foreground">
              key: <code className="rounded bg-muted px-1 py-0.5">{note.key}</code>
              {note.aliases.length > 0 && <> · 别名: {note.aliases.join(", ")}</>}
              {note.tags.length > 0 && <> · 标签: {note.tags.join(", ")}</>}
            </p>
          )}
        </CardHeader>
        <CardContent className="space-y-3">
          {error && <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>}

          <div className="space-y-1.5">
            <label htmlFor="note-title" className="text-sm font-medium">
              标题
            </label>
            <Input id="note-title" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="笔记标题" />
          </div>

          <div className="space-y-1.5">
            <label htmlFor="note-body" className="text-sm font-medium">
              正文（Markdown，支持 [[wikilink]]）
            </label>
            <textarea
              id="note-body"
              value={body}
              onChange={(e) => setBody(e.target.value)}
              placeholder="在此输入正文…"
              className="min-h-[320px] w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
              rows={18}
            />
          </div>

          <div className="flex flex-wrap gap-2">
            <Button onClick={handleSave} disabled={saving || !dirty} className="gap-2">
              <Save className="size-4" />
              {saving ? "保存中…" : "保存"}
            </Button>
            <Button variant="destructive" onClick={handleDelete} className="gap-2">
              <Trash2 className="size-4" />
              删除
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
