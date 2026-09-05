/**
 * AI 链接/标签建议对话框 —— 候选前 200 篇 + 当前笔记 → chatOnce → 解析 JSON
 */
import { useCallback, useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { listNotes } from "@/api/tauri";
import { chatOnce } from "@/lib/ai-client";
import { buildLinkSuggestMessages, parseLinkSuggestJson } from "@/lib/ai-prompts";
import { getAiConfig } from "@/lib/ai-config";
import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";

type Props = {
  noteTitle: string;
  noteBody: string;
  onClose: () => void;
  onInsertRef: (text: string) => void;
  onAppendTag: (tagText: string) => void;
  onOpenSettings: () => void;
};

export function LinkSuggestDialog({ noteTitle, noteBody, onClose, onInsertRef, onAppendTag, onOpenSettings }: Props) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [links, setLinks] = useState<string[]>([]);
  const [tags, setTags] = useState<string[]>([]);
  const [formatError, setFormatError] = useState(false);
  const [bodyForExisting, setBodyForExisting] = useState(noteBody);

  useEffect(() => {
    setBodyForExisting(noteBody);
  }, [noteBody]);

  const fetchSuggest = useCallback(async () => {
    setLoading(true);
    setError(null);
    setFormatError(false);
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl || !getToken(cfg.baseUrl)) {
      setError("未配置服务器");
      setLoading(false);
      return;
    }
    const aiCfg = getAiConfig();
    if (!aiCfg) {
      setError("未选择模型，请先在 AI 助手面板选择模型");
      setLoading(false);
      return;
    }
    try {
      const notes = await listNotes();
      const candidates = notes.slice(0, 200).map((n) => ({ key: n.key, title: n.title }));
      const messages = buildLinkSuggestMessages({ noteTitle, noteBody, candidates });
      const text = await chatOnce({ baseUrl: aiCfg.baseUrl, model: aiCfg.model, messages });
      const parsed = parseLinkSuggestJson(text);
      if (!parsed) {
        setFormatError(true);
        setLinks([]);
        setTags([]);
      } else {
        setLinks(parsed.links);
        setTags(parsed.tags);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    } finally {
      setLoading(false);
    }
  }, [noteTitle, noteBody]);

  useEffect(() => {
    void fetchSuggest();
  }, [fetchSuggest]);

  // Esc 关闭
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const existingKeys = (() => {
    const re = /\[\[([^\]|]+)(?:\|[^\]]+)?\]\]/g;
    const set = new Set<string>();
    let m: RegExpExecArray | null;
    while ((m = re.exec(bodyForExisting))) set.add(m[1].trim());
    return set;
  })();

  const overlay = (
    <div data-modal-open="" className="fixed inset-0 z-50 flex items-center justify-center bg-black/60" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="max-h-[80vh] w-[min(92vw,560px)] overflow-auto rounded-lg border border-border bg-popover p-4 shadow-xl" onMouseDown={(e) => e.stopPropagation()} onClick={(e) => e.stopPropagation()}>
        <h2 className="text-sm font-semibold">AI 建议</h2>
        <p className="mt-1 text-xs text-muted-foreground">基于当前笔记内容推荐相关链接与标签</p>

        {loading ? (
          <p className="mt-4 text-sm text-muted-foreground">生成中…</p>
        ) : error ? (
          <div className="mt-4 space-y-2">
            <p className="text-sm text-destructive">{error}</p>
            {(error.includes("未配置") || error.includes("未选择")) && (
              <Button type="button" size="sm" onClick={() => { onClose(); onOpenSettings(); }}>
                打开设置
              </Button>
            )}
            {!error.includes("未配置") && !error.includes("未选择") && (
              <Button type="button" variant="outline" size="sm" onClick={() => void fetchSuggest()}>
                重试
              </Button>
            )}
          </div>
        ) : formatError ? (
          <div className="mt-4 space-y-2">
            <p className="text-sm text-destructive">AI 返回格式异常，请重试</p>
            <Button type="button" variant="outline" size="sm" onClick={() => void fetchSuggest()}>
              重试
            </Button>
          </div>
        ) : (
          <div className="mt-4 space-y-4">
            <section>
              <h3 className="text-xs font-medium">链接建议 ({links.length})</h3>
              {links.length === 0 ? (
                <p className="mt-1 text-xs text-muted-foreground">暂无建议</p>
              ) : (
                <ul className="mt-2 space-y-1">
                  {links.map((k) => {
                    const existed = existingKeys.has(k);
                    return (
                      <li key={k} className="flex items-center justify-between rounded border px-2 py-1.5">
                        <span className="min-w-0 flex-1 truncate text-xs">
                          <code className="rounded bg-muted px-1 py-0.5">{k}</code>
                        </span>
                        <Button type="button" size="sm" variant={existed ? "ghost" : "outline"} className="ml-2 h-7 shrink-0 text-xs" disabled={existed} onClick={() => { onInsertRef(`[[${k}]]`); }}>
                          {existed ? "已存在" : "插入"}
                        </Button>
                      </li>
                    );
                  })}
                </ul>
              )}
            </section>
            <section>
              <h3 className="text-xs font-medium">标签建议 ({tags.length})</h3>
              {tags.length === 0 ? (
                <p className="mt-1 text-xs text-muted-foreground">暂无建议</p>
              ) : (
                <ul className="mt-2 flex flex-wrap gap-1.5">
                  {tags.map((t) => (
                    <li key={t} className="inline-flex items-center gap-1 rounded-full border bg-muted/40 px-2 py-1 text-xs">
                      <span>#{t}</span>
                      <Button type="button" variant="ghost" size="sm" className="h-5 px-1.5 text-xs" onClick={() => onAppendTag(` #${t}`)}>
                        添加
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          </div>
        )}

        <div className="mt-6 flex justify-end gap-2">
          {!loading && !error && !formatError && (
            <Button type="button" variant="outline" size="sm" onClick={() => void fetchSuggest()}>
              刷新
            </Button>
          )}
          <Button type="button" variant="ghost" size="sm" onClick={onClose}>
            关闭
          </Button>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
