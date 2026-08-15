import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQueryClient } from '@tanstack/react-query';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { useImeGuard } from '@/hooks/useImeGuard';
import { kbStream } from '@/api/kbStream';
import {
  useLlmKbDocs,
  useUploadKbDoc,
  useDeleteKbDoc,
  useReindexKbDoc,
  useKbQuery,
  useToggleLlmKb,
  useDeleteLlmKb,
} from '@/api/hooks';
import KbDialog from './KbDialog';
import type { LlmKnowledgeBase, LlmKbDocument } from '@/types';
import {
  ArrowLeft,
  FileUp,
  Loader2,
  RefreshCw,
  Search,
  Trash2,
  Edit3,
  FileText,
  AlertTriangle,
} from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';

const TEXT_MAX_BYTES = 2 * 1024 * 1024;
const BINARY_MAX_BYTES = 20 * 1024 * 1024;
const ACCEPTED_EXTENSIONS = ['md', 'txt', 'pdf', 'docx', 'xlsx', 'pptx'];
const TEXT_EXTENSIONS = ['md', 'txt'];

function maxBytesFor(ext: string): number {
  return TEXT_EXTENSIONS.includes(ext) ? TEXT_MAX_BYTES : BINARY_MAX_BYTES;
}

interface Props {
  kb: LlmKnowledgeBase;
  onBack: () => void;
  onDeleted: () => void;
}

interface DocOverride {
  status: string;
  chunk_count: number;
  error?: string | null;
}

function DocStatusBadge({ status }: { status: string }) {
  const { t } = useTranslation();
  let variant: 'default' | 'secondary' | 'destructive' | 'outline' = 'outline';
  let className = '';
  if (status === 'processing') {
    className = 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400';
  } else if (status === 'ready') {
    className = 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400';
  } else if (status === 'failed') {
    variant = 'destructive';
  }
  return (
    <Badge variant={variant} className={className}>
      {t(`kb.status.${status}`, { defaultValue: status })}
    </Badge>
  );
}

function DocRow({
  doc,
  onDelete,
  onReindex,
  deleting,
  reindexing,
}: {
  doc: LlmKbDocument;
  onDelete: () => void;
  onReindex: () => void;
  deleting: boolean;
  reindexing: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-between gap-3 border-b py-2 last:border-b-0">
      <div className="flex min-w-0 items-center gap-2">
        <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
        <div className="min-w-0">
          <div className="truncate text-sm font-medium">{doc.filename}</div>
          <div className="text-xs text-muted-foreground">
            {doc.chunk_count} {t('kb.chunks')}
          </div>
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <DocStatusBadge status={doc.status} />
        <Button
          variant="ghost"
          size="icon"
          onClick={onReindex}
          disabled={reindexing || deleting}
          aria-label={t('kb.reindex')}
        >
          {reindexing ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={onDelete}
          disabled={deleting || reindexing}
          aria-label={t('kb.deleteDoc', { name: doc.filename })}
        >
          {deleting ? <Loader2 className="h-4 w-4 animate-spin text-destructive" /> : <Trash2 className="h-4 w-4 text-destructive" />}
        </Button>
      </div>
    </div>
  );
}

export default function KbDetail({ kb, onBack, onDeleted }: Props) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: docs, isLoading } = useLlmKbDocs(kb.id);
  const uploadMutation = useUploadKbDoc();
  const deleteMutation = useDeleteKbDoc();
  const reindexMutation = useReindexKbDoc();
  const queryMutation = useKbQuery();
  const toggleMutation = useToggleLlmKb();
  const deleteKbMutation = useDeleteLlmKb();

  const [query, setQuery] = useState('');
  const ime = useImeGuard();
  const [dragging, setDragging] = useState(false);
  const [uploadError, setUploadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [editOpen, setEditOpen] = useState(false);
  const [overrides, setOverrides] = useState<Record<string, DocOverride>>({});
  const fileInputRef = useRef<HTMLInputElement>(null);

  // SSE 订阅：按 kb 过滤，即时更新文档状态并触发后台数据失效。
  useEffect(() => {
    const unsub = kbStream.subscribe((ev) => {
      if (ev.kb_id !== kb.id) return;
      setOverrides((prev) => ({
        ...prev,
        [ev.doc_id]: { status: ev.status, chunk_count: ev.chunk_count, error: ev.error },
      }));
      qc.invalidateQueries({ queryKey: ['llm-kb-docs', kb.id] });
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
    });
    return unsub;
  }, [kb.id, qc]);

  const effectiveDocs: LlmKbDocument[] = (docs ?? []).map((d) => {
    const o = overrides[d.id];
    return o ? { ...d, status: o.status as LlmKbDocument['status'], chunk_count: o.chunk_count, error: o.error } : d;
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
    setUploadError(hasInvalid ? t('kb.fileInvalid') : null);
    accepted.forEach((f) =>
      uploadMutation.mutate(
        { kbId: kb.id, file: f },
        {
          onError: (err) => {
            setUploadError(t('kb.uploadError', { error: getApiErrorMessage(err) }));
          },
        },
      ),
    );
  };

  const runQuery = () => {
    if (!query.trim()) return;
    queryMutation.mutate({ kbId: kb.id, text: query.trim() });
  };

  const deleteKb = () => {
    if (confirm(t('kb.deleteKb', { name: kb.name }))) {
      setActionError(null);
      deleteKbMutation.mutate(kb.id, {
        onSuccess: onDeleted,
        onError: (err) => {
          setActionError(t('kb.actionError', { error: getApiErrorMessage(err) }));
        },
      });
    }
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <Button variant="ghost" size="icon" className="lg:hidden" onClick={onBack} aria-label={t('common.close')}>
              <ArrowLeft className="h-4 w-4" />
            </Button>
            <div className="min-w-0">
              <CardTitle className="flex items-center gap-2 text-lg">
                <span className="truncate">{kb.name}</span>
                <Badge variant={kb.enabled ? 'default' : 'secondary'}>
                  {kb.enabled ? t('kb.enabled') : t('kb.disabled')}
                </Badge>
              </CardTitle>
              {kb.description && <p className="mt-1 text-sm text-muted-foreground">{kb.description}</p>}
              <p className="mt-1 text-xs text-muted-foreground">
                {kb.emb_model} · dim {kb.emb_dimension} · top_k {kb.top_k}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Switch
              checked={kb.enabled}
              onCheckedChange={(v) => toggleMutation.mutate({ id: kb.id, enabled: v })}
              aria-label={t('kb.enabledSwitch')}
            />
            <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
              <Edit3 className="mr-1 h-4 w-4" /> {t('common.edit')}
            </Button>
            <Button variant="outline" size="sm" className="text-destructive" onClick={deleteKb}>
              <Trash2 className="mr-1 h-4 w-4" /> {t('common.delete')}
            </Button>
          </div>
        </CardHeader>
      </Card>

      {actionError && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {actionError}
        </div>
      )}

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
            <span className="text-sm font-medium">{t('kb.uploadHint')}</span>
            <span className="text-xs text-muted-foreground">{t('kb.browse')}</span>
            {uploadError && <span className="text-xs text-destructive">{uploadError}</span>}
          </div>
        </CardContent>
      </Card>

      {/* 文档列表 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('kb.docs')} ({effectiveDocs.length})
          </CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          {isLoading ? (
            <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
          ) : effectiveDocs.length === 0 ? (
            <div className="text-sm text-muted-foreground">{t('kb.emptyDocs')}</div>
          ) : (
            <div>
              {effectiveDocs.map((d) => {
                const deleting = deleteMutation.isPending && deleteMutation.variables?.docId === d.id;
                const reindexing = reindexMutation.isPending && reindexMutation.variables?.docId === d.id;
                return (
                  <DocRow
                    key={d.id}
                    doc={d}
                    deleting={deleting}
                    reindexing={reindexing}
                    onDelete={() => {
                      if (confirm(t('kb.deleteDoc', { name: d.filename }))) {
                        setActionError(null);
                        deleteMutation.mutate(
                          { kbId: kb.id, docId: d.id },
                          {
                            onError: (err) => {
                              setActionError(t('kb.actionError', { error: getApiErrorMessage(err) }));
                            },
                          },
                        );
                      }
                    }}
                    onReindex={() => {
                      if (confirm(t('kb.reindexConfirm', { name: d.filename }))) {
                        setActionError(null);
                        reindexMutation.mutate(
                          { kbId: kb.id, docId: d.id },
                          {
                            onError: (err) => {
                              setActionError(t('kb.actionError', { error: getApiErrorMessage(err) }));
                            },
                          },
                        );
                      }
                    }}
                  />
                );
              })}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 检索预览 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('kb.query')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 p-4">
          <div className="flex gap-2">
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              {...ime.bind}
              onKeyDown={(e) => {
                // IME 组词中回车是确认候选，不触发检索
                if (ime.isComposing(e)) return;
                if (e.key === 'Enter') runQuery();
              }}
              placeholder={t('kb.queryPlaceholder')}
            />
            <Button onClick={runQuery} disabled={queryMutation.isPending || !query.trim()}>
              {queryMutation.isPending ? (
                <Loader2 className="mr-1 h-4 w-4 animate-spin" />
              ) : (
                <Search className="mr-1 h-4 w-4" />
              )}
              {t('kb.queryButton')}
            </Button>
          </div>
          {queryMutation.isPending && <div className="text-sm text-muted-foreground">{t('common.loading')}</div>}
          {queryMutation.isError && <div className="text-sm text-destructive">{t('kb.queryError')}</div>}
          {queryMutation.data && queryMutation.data.chunks.length === 0 && (
            <div className="text-sm text-muted-foreground">{t('kb.noResults')}</div>
          )}
          {queryMutation.data && queryMutation.data.chunks.length > 0 && (
            <div className="max-h-80 space-y-2 overflow-y-auto pr-1">
              {queryMutation.data.chunks.map((c, i) => (
                <div key={i} className="rounded-lg border bg-muted/30 p-3">
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate text-xs font-medium text-muted-foreground">
                      {c.heading_path || '—'}
                    </span>
                    <Badge variant="outline" className="shrink-0">
                      {t('kb.score')}: {c.score.toFixed(3)}
                    </Badge>
                  </div>
                  <p className="mt-1 text-xs leading-relaxed">{c.content}</p>
                </div>
              ))}
            </div>
          )}
          {!queryMutation.data && !queryMutation.isPending && !queryMutation.isError && (
            <div className="text-xs text-muted-foreground">{t('kb.queryEmpty')}</div>
          )}
        </CardContent>
      </Card>

      <KbDialog open={editOpen} onClose={() => setEditOpen(false)} kbId={kb.id} />
    </div>
  );
}
