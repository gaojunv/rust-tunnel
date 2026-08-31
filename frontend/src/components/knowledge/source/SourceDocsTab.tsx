import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQueryClient } from '@tanstack/react-query';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { FileText, Loader2, RefreshCw, Trash2, AlertTriangle, ChevronDown } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { knowledgeStream } from '@/api/knowledgeStream';
import {
  useKnowledgeDocs,
  useUploadKnowledgeDoc,
  useDeleteKnowledgeDoc,
  useReindexKnowledgeDoc,
} from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import type {
  KnowledgeSource,
  KnowledgeDoc,
  KnowledgeDocIndexState,
  KnowledgeIndexKind,
} from '@/types';
import DocUploadZone, { PROCESSING_TTL_MS } from '@/components/knowledge/shared/DocUploadZone';

function statusVariant(status: string): 'default' | 'secondary' | 'destructive' | 'outline' {
  if (status === 'ready') return 'default';
  if (status === 'processing' || status === 'pending') return 'secondary';
  if (status === 'failed') return 'destructive';
  return 'outline';
}

function statusClass(status: string): string {
  if (status === 'processing') {
    return 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400';
  }
  if (status === 'ready') {
    return 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400';
  }
  // idle 沿用 outline 的中性样式（与 pending 前的占位一致），有文案即可区分
  return '';
}

function DocSideBadge({ state, countLabel }: { state: KnowledgeDocIndexState; countLabel: string }) {
  const { t } = useTranslation();
  return (
    <span className="flex items-center gap-1">
      <Badge variant={statusVariant(state.status)} className={statusClass(state.status)}>
        {t(`ks.status.${state.status}`, { defaultValue: state.status })}
      </Badge>
      <span className="text-xs text-muted-foreground">{countLabel}</span>
    </span>
  );
}

/** 单文档行：文件名 + 类型/时间 + 每侧状态徽章 + 重建/删除。 */
function DocRow({
  doc,
  source,
  deleting,
  reindexing,
  onDelete,
  onReindex,
}: {
  doc: KnowledgeDoc;
  source: KnowledgeSource;
  deleting: boolean;
  reindexing: boolean;
  onDelete: () => void;
  onReindex: (kind?: KnowledgeIndexKind) => void;
}) {
  const { t } = useTranslation();
  const bothEnabled = source.index_vector && source.index_pages;

  // 两侧可能各自失败且原因不同，故按侧分行并标注侧名，不合并成一条
  const sideErrors = [
    doc.vector?.status === 'failed' ? { label: t('ks.badgeVector'), error: doc.vector.error } : null,
    doc.pages?.status === 'failed' ? { label: t('ks.badgePages'), error: doc.pages.error } : null,
  ].filter((e): e is { label: string; error: string } => Boolean(e?.error));

  return (
    <div className="flex flex-wrap items-center justify-between gap-2 border-b py-2 last:border-b-0">
      <div className="flex min-w-0 items-center gap-2">
        <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
        <div className="min-w-0">
          <div className="truncate text-sm font-medium" title={doc.filename}>
            {doc.filename}
          </div>
          <div className="text-xs text-muted-foreground">
            {doc.file_type}
            {' · '}
            {new Date(doc.created_at).toLocaleDateString()}
          </div>
          {sideErrors.map((e) => (
            <div key={e.label} className="break-words text-xs text-destructive">
              {e.label}: {e.error}
            </div>
          ))}
        </div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        {doc.vector && (
          <DocSideBadge
            state={doc.vector}
            countLabel={t('ks.chunkCount', { count: doc.vector.chunk_count ?? 0 })}
          />
        )}
        {doc.pages && (
          <DocSideBadge
            state={doc.pages}
            countLabel={t('ks.pageCount', { count: doc.pages.page_count ?? 0 })}
          />
        )}
        {bothEnabled ? (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                disabled={reindexing || deleting}
                aria-label={t('ks.reindex')}
              >
                {reindexing ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <RefreshCw className="h-4 w-4" />
                )}
                <ChevronDown className="ml-1 h-3 w-3 opacity-60" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={() => onReindex(undefined)}>{t('ks.reindexAll')}</DropdownMenuItem>
              <DropdownMenuItem onClick={() => onReindex('vector')}>{t('ks.reindexVector')}</DropdownMenuItem>
              <DropdownMenuItem onClick={() => onReindex('pages')}>{t('ks.reindexPages')}</DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        ) : (
          <Button
            variant="ghost"
            size="icon"
            onClick={() => onReindex(undefined)}
            disabled={reindexing || deleting}
            aria-label={t('ks.reindex')}
          >
            {reindexing ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
          </Button>
        )}
        <Button
          variant="ghost"
          size="icon"
          onClick={onDelete}
          disabled={deleting || reindexing}
          aria-label={t('ks.deleteDoc', { name: doc.filename })}
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
}

export default function SourceDocsTab({ source }: { source: KnowledgeSource }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: docs, isLoading } = useKnowledgeDocs(source.id);
  const uploadMutation = useUploadKnowledgeDoc();
  const deleteMutation = useDeleteKnowledgeDoc();
  const reindexMutation = useReindexKnowledgeDoc();

  const [actionError, setActionError] = useState<string | null>(null);
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const [overrides, setOverrides] = useState<
    Record<string, { vector?: KnowledgeDocIndexState; pages?: KnowledgeDocIndexState }>
  >({});
  // per-doc+kind 的 processing 过期定时器（key=`${doc_id}:${kind}`）：SSE 终态丢失时解除假卡
  const overrideTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  // SSE 订阅：按 source 过滤，即时更新文档状态并触发后台失效；Lagged 时重拉列表
  useEffect(() => {
    const timers = overrideTimersRef.current;
    const unsub = knowledgeStream.subscribe({
      onEvent: (ev) => {
        if (ev.kb_id !== source.id) return;
        // 事件的 chunk_count 是「该侧的条目数」，落到各侧自己的计数字段
        const side: KnowledgeDocIndexState =
          ev.kind === 'vector'
            ? { status: ev.status, chunk_count: ev.chunk_count, error: ev.error ?? null }
            : { status: ev.status, page_count: ev.chunk_count, error: ev.error ?? null };
        setOverrides((prev) => ({
          ...prev,
          [ev.doc_id]: { ...prev[ev.doc_id], [ev.kind]: side },
        }));
        const key = `${ev.doc_id}:${ev.kind}`;
        const prevTimer = timers.get(key);
        if (prevTimer) {
          clearTimeout(prevTimer);
          timers.delete(key);
        }
        if (ev.status === 'processing') {
          timers.set(
            key,
            setTimeout(() => {
              timers.delete(key);
              setOverrides((prev) => {
                const cur = prev[ev.doc_id];
                // 触发时若已被后续事件改为终态则不动（防御；正常路径下终态已清定时器）
                if (cur?.[ev.kind]?.status !== 'processing') return prev;
                const entry = { ...cur };
                delete entry[ev.kind];
                const next = { ...prev };
                if (entry.vector || entry.pages) {
                  next[ev.doc_id] = entry;
                } else {
                  delete next[ev.doc_id];
                }
                return next;
              });
              qc.invalidateQueries({ queryKey: ['knowledge-docs', source.id] });
            }, PROCESSING_TTL_MS),
          );
        }
        qc.invalidateQueries({ queryKey: ['knowledge-docs', source.id] });
        qc.invalidateQueries({ queryKey: ['knowledge-sources'] });
        if (ev.kind === 'pages') {
          qc.invalidateQueries({ queryKey: ['agent-wiki-pages', source.id] });
          qc.invalidateQueries({ queryKey: ['agent-wiki-graph', source.id] });
        }
      },
      onSync: () => {
        qc.invalidateQueries({ queryKey: ['knowledge-docs', source.id] });
        qc.invalidateQueries({ queryKey: ['knowledge-sources'] });
      },
    });
    return () => {
      unsub();
      timers.forEach((timer) => clearTimeout(timer));
      timers.clear();
    };
  }, [source.id, qc]);

  const effectiveDocs: KnowledgeDoc[] = (docs ?? []).map((d) => {
    const o = overrides[d.id];
    if (!o) return d;
    // 只覆盖容器已启用的侧：`null` 严格表示该侧未启用，若为它套 override 会凭空造出徽章
    return {
      ...d,
      vector: d.vector && o.vector ? { ...d.vector, ...o.vector } : d.vector,
      pages: d.pages && o.pages ? { ...d.pages, ...o.pages } : d.pages,
    };
  });

  return (
    <div className="space-y-4">
      <DocUploadZone
        isUploading={uploadMutation.isPending}
        labels={{ uploadHint: t('ks.uploadHint'), browse: t('ks.browse'), fileInvalid: t('ks.fileInvalid') }}
        onUpload={(file) => uploadMutation.mutateAsync({ sourceId: source.id, file })}
        formatUploadError={(err) => t('ks.uploadError', { error: getApiErrorMessage(err) })}
      />

      {actionError && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {actionError}
        </div>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('ks.docs')} ({effectiveDocs.length})
          </CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          {isLoading ? (
            <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
          ) : effectiveDocs.length === 0 ? (
            <div className="text-sm text-muted-foreground">{t('ks.emptyDocs')}</div>
          ) : (
            <div>
              {effectiveDocs.map((d) => {
                const deleting =
                  deleteMutation.isPending &&
                  deleteMutation.variables?.docId === d.id &&
                  deleteMutation.variables?.sourceId === source.id;
                const reindexing =
                  reindexMutation.isPending &&
                  reindexMutation.variables?.docId === d.id &&
                  reindexMutation.variables?.sourceId === source.id;
                return (
                  <DocRow
                    key={d.id}
                    doc={d}
                    source={source}
                    deleting={Boolean(deleting)}
                    reindexing={Boolean(reindexing)}
                    onDelete={() =>
                      confirm(
                        { title: t('common.confirm'), description: t('ks.deleteDoc', { name: d.filename }) },
                        () => {
                          setActionError(null);
                          deleteMutation.mutate(
                            { sourceId: source.id, docId: d.id },
                            {
                              onError: (err) => {
                                setActionError(t('ks.actionError', { error: getApiErrorMessage(err) }));
                              },
                            },
                          );
                        },
                      )
                    }
                    onReindex={(kind) => {
                      const desc =
                        kind == null
                          ? t('ks.reindexConfirm', { name: d.filename })
                          : t('ks.reindexKindConfirm', {
                              name: d.filename,
                              kind: kind === 'vector' ? t('ks.badgeVector') : t('ks.badgePages'),
                            });
                      confirm({ title: t('common.confirm'), description: desc }, () => {
                        setActionError(null);
                        reindexMutation.mutate(
                          { sourceId: source.id, docId: d.id, kind },
                          {
                            onError: (err) => {
                              setActionError(t('ks.actionError', { error: getApiErrorMessage(err) }));
                            },
                          },
                        );
                      });
                    }}
                  />
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
