import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Switch } from '@/components/ui/switch';
import { Input } from '@/components/ui/input';
import { useImeGuard } from '@/hooks/useImeGuard';
import {
  ArrowLeft,
  Edit3,
  Loader2,
  Trash2,
  AlertTriangle,
  Search,
  Database,
  FileStack,
} from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import {
  useDeleteKnowledgeSource,
  useToggleKnowledgeSource,
  useKnowledgeQuery,
  useWikiGraph,
} from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import SourceDocsTab from './SourceDocsTab';
import SourceDialog from './SourceDialog';
import WikiPagesTab from '../wiki/WikiPagesTab';
import WikiGraph from '../wiki/WikiGraph';
import type { KnowledgeSource } from '@/types';

interface Props {
  source: KnowledgeSource;
  onBack: () => void;
  onDeleted: () => void;
}

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
  return '';
}

/** 统一知识容器详情：头部（状态/双索引徽章/总闸/编辑/删除）+ 动态 Tabs（文档/检索/页面/图谱）。
 *  图谱点击节点 → 切到页面 Tab 并打开该 ref（key 变化触发 PagesTab 重挂载）。 */
export default function SourceDetail({ source, onBack, onDeleted }: Props) {
  const { t } = useTranslation();
  const deleteMutation = useDeleteKnowledgeSource();
  const toggleMutation = useToggleKnowledgeSource();
  const queryMutation = useKnowledgeQuery();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const [editOpen, setEditOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState('docs');
  const [openReq, setOpenReq] = useState<string | null>(null);

  const [query, setQuery] = useState('');
  const ime = useImeGuard();

  // 根据索引开关动态构造 tab 列表；关掉一侧后不留空 tab
  const tabs = useMemo(() => {
    const list: { value: string; label: string }[] = [{ value: 'docs', label: t('ks.tabDocs') }];
    if (source.index_vector) list.push({ value: 'query', label: t('ks.tabQuery') });
    if (source.index_pages) {
      list.push({ value: 'pages', label: t('ks.tabPages') });
      list.push({ value: 'graph', label: t('ks.tabGraph') });
    }
    return list;
  }, [source.index_vector, source.index_pages, t]);

  // 开关变化后若当前 tab 已不可用，回落到 docs
  useEffect(() => {
    if (!tabs.some((tab) => tab.value === activeTab)) {
      setActiveTab(tabs[0]?.value ?? 'docs');
    }
  }, [tabs, activeTab]);

  const graph = useWikiGraph(activeTab === 'graph' ? source.id : '');

  const handleNodeClick = (ref: string) => {
    setOpenReq(ref);
    setActiveTab('pages');
  };

  const remove = () => {
    confirm(
      { title: t('common.confirm'), description: t('ks.deleteConfirm', { name: source.name }) },
      () => {
        setError(null);
        deleteMutation.mutate(source.id, {
          onSuccess: onDeleted,
          onError: (err) => {
            setError(t('ks.actionError', { error: getApiErrorMessage(err) }));
          },
        });
      },
    );
  };

  const runQuery = () => {
    if (!query.trim()) return;
    queryMutation.mutate({ sourceId: source.id, text: query.trim() });
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
              <CardTitle className="flex flex-wrap items-center gap-2 text-lg">
                <span className="truncate">{source.name}</span>
                <Badge variant={statusVariant(source.status)} className={statusClass(source.status)}>
                  {t(`ks.status.${source.status}`, { defaultValue: source.status })}
                </Badge>
                {/* 双索引徽章配色/图标与列表卡保持一致，切进详情时视觉不跳 */}
                {source.index_vector && (
                  <Badge
                    variant="outline"
                    className="shrink-0 gap-1 border-sky-500/40 text-sky-600 dark:text-sky-400"
                  >
                    <Database className="h-3 w-3" />
                    {t('ks.badgeVector')}
                  </Badge>
                )}
                {source.index_pages && (
                  <Badge
                    variant="outline"
                    className="shrink-0 gap-1 border-violet-500/40 text-violet-600 dark:text-violet-400"
                  >
                    <FileStack className="h-3 w-3" />
                    {t('ks.badgePages')}
                  </Badge>
                )}
              </CardTitle>
              {source.summary && <p className="mt-1 text-sm text-muted-foreground">{source.summary}</p>}
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`ks.scope_${source.scope_type}`)}
                {source.client_id && ` · ${source.client_id}`}
                {source.workspace_id && ` · ${source.workspace_id}`}
                {' · '}
                {t('ks.docCount', { count: source.doc_count })}
                {source.index_pages ? ` · ${t('ks.pageCount', { count: source.page_count })}` : ''}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Switch
              checked={source.enabled}
              onCheckedChange={(v) =>
                toggleMutation.mutate({ id: source.id, enabled: v }, { onError: (err) => setError(t('ks.actionError', { error: getApiErrorMessage(err) })) })
              }
              aria-label={t('ks.enabledSwitch')}
            />
            <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
              <Edit3 className="mr-1 h-4 w-4" /> {t('common.edit')}
            </Button>
            <Button variant="outline" size="sm" className="text-destructive" onClick={remove}>
              {deleteMutation.isPending ? (
                <Loader2 className="mr-1 h-4 w-4 animate-spin" />
              ) : (
                <Trash2 className="mr-1 h-4 w-4" />
              )}
              {t('common.delete')}
            </Button>
          </div>
        </CardHeader>
      </Card>

      {!source.enabled && (
        <div className="flex items-center gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-700 dark:text-amber-400">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {t('ks.disabled')} — {t('ks.enabledDesc')}
        </div>
      )}

      {error && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {error}
        </div>
      )}

      <Tabs value={activeTab} onValueChange={setActiveTab}>
        <TabsList>
          {tabs.map((tab) => (
            <TabsTrigger key={tab.value} value={tab.value}>
              {tab.label}
            </TabsTrigger>
          ))}
        </TabsList>
        <TabsContent value="docs" className="mt-4" forceMount>
          <SourceDocsTab source={source} />
        </TabsContent>
        {source.index_vector && (
          <TabsContent value="query" className="mt-4" forceMount>
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('ks.query')}</CardTitle>
              </CardHeader>
              <CardContent className="space-y-3 p-4">
                <div className="flex gap-2">
                  <Input
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                    {...ime.bind}
                    onKeyDown={(e) => {
                      if (ime.isComposing(e)) return;
                      if (e.key === 'Enter') runQuery();
                    }}
                    placeholder={t('ks.queryPlaceholder')}
                  />
                  <Button onClick={runQuery} disabled={queryMutation.isPending || !query.trim()}>
                    {queryMutation.isPending ? (
                      <Loader2 className="mr-1 h-4 w-4 animate-spin" />
                    ) : (
                      <Search className="mr-1 h-4 w-4" />
                    )}
                    {t('ks.queryButton')}
                  </Button>
                </div>
                {queryMutation.isPending && <div className="text-sm text-muted-foreground">{t('common.loading')}</div>}
                {queryMutation.isError && <div className="text-sm text-destructive">{t('ks.queryError')}</div>}
                {queryMutation.data && queryMutation.data.chunks.length === 0 && (
                  <div className="text-sm text-muted-foreground">{t('ks.noResults')}</div>
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
                            {t('ks.score')}: {c.score.toFixed(3)}
                          </Badge>
                        </div>
                        <p className="mt-1 text-xs leading-relaxed">{c.content}</p>
                      </div>
                    ))}
                  </div>
                )}
                {!queryMutation.data && !queryMutation.isPending && !queryMutation.isError && (
                  <div className="text-xs text-muted-foreground">{t('ks.queryEmpty')}</div>
                )}
              </CardContent>
            </Card>
          </TabsContent>
        )}
        {source.index_pages && (
          <>
            <TabsContent value="pages" className="mt-4" forceMount>
              <WikiPagesTab
                wikiId={source.id}
                defaultOpenRef={openReq}
              />
            </TabsContent>
            <TabsContent value="graph" className="mt-4" forceMount>
              <WikiGraph
                nodes={graph.data?.nodes ?? []}
                edges={graph.data?.edges ?? []}
                loading={graph.isLoading}
                onNodeClick={handleNodeClick}
              />
            </TabsContent>
          </>
        )}
      </Tabs>

      <SourceDialog open={editOpen} onClose={() => setEditOpen(false)} source={source} />
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
