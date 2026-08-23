import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQueryClient } from '@tanstack/react-query';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { FileUp, FileText, Loader2, RefreshCw, Trash2, AlertTriangle } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { wikiStream } from '@/api/wikiStream';
import { useWikiDocs, useUploadWikiDoc, useDeleteWikiDoc, useReindexWikiDoc } from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import type { WikiDocument, WikiStatus } from '@/types';

const TEXT_MAX_BYTES = 2 * 1024 * 1024;
const BINARY_MAX_BYTES = 20 * 1024 * 1024;
const ACCEPTED_EXTENSIONS = ['md', 'txt', 'pdf', 'docx', 'xlsx', 'pptx'];
const TEXT_EXTENSIONS = ['md', 'txt'];
/** 「正在处理」覆盖状态的过期 TTL：SSE 终态事件丢失（断线/丢帧）时，processing
 *  override 会永久假卡。30s 后移除 override 并失效文档查询，让 UI 回退到服务端
 *  DB 状态（真实 status），用户也可手动重试/刷新。（照 KbDetail PROCESSING_TTL_MS） */
const PROCESSING_TTL_MS = 30_000;

function maxBytesFor(ext: string): number {
  return TEXT_EXTENSIONS.includes(ext) ? TEXT_MAX_BYTES : BINARY_MAX_BYTES;
}

function DocStatusBadge({ status }: { status: string }) {
  const { t } = useTranslation();
  let variant: 'default' | 'secondary' | 'destructive' | 'outline' = 'outline';
  let className = '';
  if (status === 'processing') {
    variant = 'secondary';
    className = 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400';
  } else if (status === 'ready') {
    className = 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400';
  } else if (status === 'failed') {
    variant = 'destructive';
  } else if (status === 'pending') {
    variant = 'secondary';
  }
  return (
    <Badge variant={variant} className={className}>
      {t(`wiki.status.${status}`, { defaultValue: status })}
    </Badge>
  );
}

interface DocOverride {
  status: WikiStatus;
  error?: string | null;
}

/** Wiki 文档管理子 Tab：上传（拖拽/点击）+ 列表（状态 badge/reindex/删除）。
 *  wikiStream 订阅驱动状态实时更新；Lagged(sync) 时重拉列表。 */
export default function WikiDocsTab({ wikiId }: { wikiId: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: docs, isLoading } = useWikiDocs(wikiId);
  const uploadMutation = useUploadWikiDoc();
  const deleteMutation = useDeleteWikiDoc();
  const reindexMutation = useReindexWikiDoc();

  const [dragging, setDragging] = useState(false);
  const [uploadError, setUploadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const [overrides, setOverrides] = useState<Record<string, DocOverride>>({});
  const fileInputRef = useRef<HTMLInputElement>(null);
  // per-doc 的 processing 过期定时器（doc_id → timer）：SSE 终态事件丢失时解除
  // 永久 processing 假卡（详见 PROCESSING_TTL_MS 注释）。
  const overrideTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  // SSE 订阅：按 wiki 过滤，即时更新文档状态并触发后台数据失效。
  useEffect(() => {
    // 快照 ref 当前值供 cleanup 使用（ref 本身在 effect 生命周期内恒定）
    const timers = overrideTimersRef.current;
    const unsub = wikiStream.subscribe({
      onWiki: (ev) => {
        if (ev.wiki_id !== wikiId) return;
        setOverrides((prev) => ({
          ...prev,
          [ev.doc_id]: { status: ev.status, error: ev.error },
        }));
        // processing 是过渡态：每收到新事件都重排/取消过期定时器。终态（ready/
        // failed）清除定时器；processing 则安排 TTL——定时器触发时移除 override，
        // 让 effectiveDocs 回退到 docs 查询（服务端 DB 真实状态）。
        const prevTimer = timers.get(ev.doc_id);
        if (prevTimer) {
          clearTimeout(prevTimer);
          timers.delete(ev.doc_id);
        }
        if (ev.status === 'processing') {
          timers.set(
            ev.doc_id,
            setTimeout(() => {
              timers.delete(ev.doc_id);
              setOverrides((prev) => {
                // 触发时若已被后续事件改为终态则不动（防御；正常路径下终态已清定时器）
                if (!prev[ev.doc_id] || prev[ev.doc_id].status !== 'processing') return prev;
                const next = { ...prev };
                delete next[ev.doc_id];
                return next;
              });
              // 失效文档查询：移除 override 后 UI 回退到服务端 DB 状态而非陈旧缓存
              qc.invalidateQueries({ queryKey: ['agent-wiki-docs', wikiId] });
            }, PROCESSING_TTL_MS),
          );
        }
        qc.invalidateQueries({ queryKey: ['agent-wiki-docs', wikiId] });
        qc.invalidateQueries({ queryKey: ['agent-wikis'] });
      },
      onSync: () => {
        // Lagged：广播槽溢出丢事件，强制重拉列表以获得完整状态
        qc.invalidateQueries({ queryKey: ['agent-wiki-docs', wikiId] });
        qc.invalidateQueries({ queryKey: ['agent-wikis'] });
      },
    });
    return () => {
      unsub();
      // 卸载时清掉所有过期定时器，避免对已卸载组件 setState
      timers.forEach((timer) => clearTimeout(timer));
      timers.clear();
    };
  }, [wikiId, qc]);

  const effectiveDocs: WikiDocument[] = (docs?.documents ?? []).map((d) => {
    const o = overrides[d.id];
    return o ? { ...d, status: o.status, error: o.error } : d;
  });

  const handleFiles = (list: FileList | null) => {
    if (!list || list.length === 0) return;
    let hasInvalid = false;
    const accepted: File[] = [];
    Array.from(list).forEach((f) => {
      const ext = f.name.toLowerCase().split('.').pop() ?? '';
      if (ACCEPTED_EXTENSIONS.includes(ext) && f.size <= maxBytesFor(ext)) {
        accepted.push(f);
      } else {
        hasInvalid = true;
      }
    });
    setUploadError(hasInvalid ? t('wiki.fileInvalid') : null);
    accepted.forEach((f) =>
      uploadMutation.mutate(
        { wikiId, file: f },
        {
          onError: (err) => {
            setUploadError(t('wiki.uploadError', { error: getApiErrorMessage(err) }));
          },
        },
      ),
    );
  };

  return (
    <div className="space-y-4">
      {/* 上传区 */}
      <Card>
        <CardContent className="p-4">
          <input
            ref={fileInputRef}
            type="file"
            accept=".md,.txt,.pdf,.docx,.xlsx,.pptx"
            multiple
            className="hidden"
            onChange={(e) => {
              handleFiles(e.target.files);
              e.target.value = '';
            }}
          />
          <div
            className={`flex flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed p-6 text-center transition-colors ${
              dragging ? 'border-primary bg-primary/5' : 'border-border'
            }`}
            onDragOver={(e) => {
              e.preventDefault();
              setDragging(true);
            }}
            onDragLeave={() => setDragging(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragging(false);
              handleFiles(e.dataTransfer.files);
            }}
            onClick={() => fileInputRef.current?.click()}
            role="button"
          >
            {uploadMutation.isPending ? (
              <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
            ) : (
              <FileUp className="h-6 w-6 text-muted-foreground" />
            )}
            <span className="text-sm font-medium">{t('wiki.uploadHint')}</span>
            <span className="text-xs text-muted-foreground">{t('wiki.browse')}</span>
            {uploadError && <span className="text-xs text-destructive">{uploadError}</span>}
          </div>
        </CardContent>
      </Card>

      {actionError && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {actionError}
        </div>
      )}

      {/* 文档列表 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('wiki.docs')} ({effectiveDocs.length})
          </CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          {isLoading ? (
            <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
          ) : effectiveDocs.length === 0 ? (
            <div className="text-sm text-muted-foreground">{t('wiki.emptyDocs')}</div>
          ) : (
            <div>
              {effectiveDocs.map((d) => {
                const deleting = deleteMutation.isPending && deleteMutation.variables?.docId === d.id;
                const reindexing = reindexMutation.isPending && reindexMutation.variables?.docId === d.id;
                return (
                  <div key={d.id} className="flex flex-wrap items-center justify-between gap-2 border-b py-2 last:border-b-0">
                    <div className="flex min-w-0 items-center gap-2">
                      <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium">{d.filename}</div>
                        {d.error && (
                          <div className="truncate text-xs text-destructive">{d.error}</div>
                        )}
                      </div>
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                      <DocStatusBadge status={d.status} />
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          confirm(
                            { title: t('common.confirm'), description: t('wiki.reindexConfirm', { name: d.filename }) },
                            () => {
                              setActionError(null);
                              reindexMutation.mutate(
                                { wikiId, docId: d.id },
                                {
                                  onError: (err) => {
                                    setActionError(t('wiki.actionError', { error: getApiErrorMessage(err) }));
                                  },
                                },
                              );
                            },
                          )
                        }
                        disabled={reindexing || deleting}
                        aria-label={t('wiki.reindex')}
                      >
                        {reindexing ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          confirm(
                            { title: t('common.confirm'), description: t('wiki.deleteDoc', { name: d.filename }) },
                            () => {
                              setActionError(null);
                              deleteMutation.mutate(
                                { wikiId, docId: d.id },
                                {
                                  onError: (err) => {
                                    setActionError(t('wiki.actionError', { error: getApiErrorMessage(err) }));
                                  },
                                },
                              );
                            },
                          )
                        }
                        disabled={deleting || reindexing}
                        aria-label={t('wiki.deleteDoc', { name: d.filename })}
                      >
                        {deleting ? (
                          <Loader2 className="h-4 w-4 animate-spin text-destructive" />
                        ) : (
                          <Trash2 className="h-4 w-4 text-destructive" />
                        )}
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </CardContent>
      </Card>

      <ConfirmDialog
        open={confirmOpen}
        payload={confirmPayload}
        onConfirm={confirmAndClose}
        onCancel={cancelConfirm}
        variant="destructive"
      />
    </div>
  );
}
