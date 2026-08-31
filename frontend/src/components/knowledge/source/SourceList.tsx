import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Badge } from '@/components/ui/badge';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Database, FileStack } from 'lucide-react';
import { useKnowledgeStream } from '@/api/hooks';
import { listKnowledgeSources } from '@/api/client';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import LoadMoreFooter from '@/components/knowledge/shared/LoadMoreFooter';
import { usePagedList } from '@/components/knowledge/shared/usePagedList';
import type { AgentMemoryScope, KnowledgeIndexKind, KnowledgeSource } from '@/types';

/** 统一容器列表的 UI 过滤条件（父组件持有，映射到 `/api/knowledge` 查询参数）。 */
export interface SourceFilters {
  indexKind: 'all' | KnowledgeIndexKind;
  scope: 'all' | AgentMemoryScope;
  clientId: string;
  workspaceId: string;
  q: string;
  status: string;
}

export const EMPTY_SOURCE_FILTERS: SourceFilters = {
  indexKind: 'all',
  scope: 'all',
  clientId: '',
  workspaceId: '',
  q: '',
  status: '',
};

interface Props {
  filters: SourceFilters;
  onFiltersChange: (filters: SourceFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onSettings?: () => void;
  /** 兼容旧调用：若传入则直接展示，不再分页拉取。 */
  sources?: KnowledgeSource[];
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

function CompactRow({ s, selected, onSelect }: { s: KnowledgeSource; selected: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  return (
    <div
      onClick={onSelect}
      className={cn(
        'flex h-12 cursor-pointer items-center justify-between gap-2 rounded-md border px-3 transition-colors hover:border-primary/40',
        selected && 'border-primary/60 bg-primary/5',
        !s.enabled && 'opacity-60',
      )}
    >
      <div className="flex min-w-0 items-center gap-2">
        <span className="truncate text-sm font-medium">{s.name}</span>
        <Badge variant={statusVariant(s.status)} className={cn('shrink-0 text-[10px]', statusClass(s.status))}>
          {t(`ks.status.${s.status}`)}
        </Badge>
        {s.index_vector && (
          <Badge variant="outline" className="hidden shrink-0 gap-1 border-sky-500/40 text-sky-600 dark:text-sky-400 sm:inline-flex">
            <Database className="h-3 w-3" />
            {t('ks.badgeVector')}
          </Badge>
        )}
        {s.index_pages && (
          <Badge variant="outline" className="hidden shrink-0 gap-1 border-violet-500/40 text-violet-600 dark:text-violet-400 sm:inline-flex">
            <FileStack className="h-3 w-3" />
            {t('ks.badgePages')}
          </Badge>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-2 text-xs text-muted-foreground">
        <span>{s.doc_count}</span>
        {!s.enabled && <Badge variant="secondary" className="text-[10px]">{t('ks.disabled')}</Badge>}
      </div>
    </div>
  );
}

export default function SourceList({
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
  onSettings,
  sources: sourcesProp,
}: Props) {
  const { t } = useTranslation();
  useKnowledgeStream();

  const searching = (filters.q ?? '').trim().length > 0;
  const filtersKey = JSON.stringify(filters);

  const fetchPage = useCallback(async (offset: number, limit: number) => {
    const res = await listKnowledgeSources({
      index_kind: filters.indexKind === 'all' ? undefined : filters.indexKind,
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: (filters.q ?? '').trim() || undefined,
      status: filters.status || undefined,
      limit,
      offset,
    });
    return { items: res.sources, total: res.total };
  }, [filters.indexKind, filters.scope, filters.clientId, filters.workspaceId, filters.q, filters.status]);

  const paged = usePagedList<KnowledgeSource>({ fetchPage, filtersKey, pageSize: 20 });
  const usingPaged = sourcesProp === undefined;
  const sources = usingPaged ? paged.items : sourcesProp!;
  const total = usingPaged ? paged.total : sourcesProp!.length;

  return (
    <SectionFrame
      title={t('ks.listTitle')}
      count={total}
      newLabel={t('ks.new')}
      onNew={onNew}
      onSettings={onSettings}
      settingsLabel={t('knowledge.sharedEmbeddingTitle')}
    >
      <ScopeFilterBar
        scope={filters.scope}
        clientId={filters.clientId}
        workspaceId={filters.workspaceId}
        q={filters.q}
        scopeLabelKey="ks.scopeLabel"
        searchPlaceholderKey="ks.searchPlaceholder"
        clientLabelKey="ks.clientLabel"
        workspaceLabelKey="ks.workspaceLabel"
        clientPlaceholderKey="ks.clientPlaceholder"
        workspacePlaceholderKey="ks.workspacePlaceholder"
        onScopeChange={(scope) => onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' })}
        onClientChange={(clientId) => onFiltersChange({ ...filters, clientId })}
        onWorkspaceChange={(workspaceId) => onFiltersChange({ ...filters, workspaceId })}
        onSearchChange={(q) => onFiltersChange({ ...filters, q })}
        extra={
          <div className="flex items-center gap-2">
            <Select
              value={filters.indexKind}
              onValueChange={(v) => onFiltersChange({ ...filters, indexKind: v as SourceFilters['indexKind'] })}
            >
              <SelectTrigger className="h-9 flex-1" aria-label={t('ks.indexKindFilter')}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">{t('ks.indexKindAll')}</SelectItem>
                <SelectItem value="vector">{t('ks.indexKindVector')}</SelectItem>
                <SelectItem value="pages">{t('ks.indexKindPages')}</SelectItem>
              </SelectContent>
            </Select>
            <Select
              value={filters.status || '__all__'}
              onValueChange={(v) => onFiltersChange({ ...filters, status: v === '__all__' ? '' : v })}
            >
              <SelectTrigger className="h-9 flex-1" aria-label={t('ks.statusFilter')}>
                <SelectValue placeholder={t('ks.statusAll')} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="__all__">{t('ks.statusAll')}</SelectItem>
                <SelectItem value="draft">{t('ks.status.draft')}</SelectItem>
                <SelectItem value="pending">{t('ks.status.pending')}</SelectItem>
                <SelectItem value="processing">{t('ks.status.processing')}</SelectItem>
                <SelectItem value="ready">{t('ks.status.ready')}</SelectItem>
                <SelectItem value="failed">{t('ks.status.failed')}</SelectItem>
              </SelectContent>
            </Select>
          </div>
        }
      />

      {sources.length === 0 ? (
        paged.loading && usingPaged ? (
          <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
        ) : (
          <div className="rounded-lg border p-6 text-center text-sm text-muted-foreground">
            {searching ? t('ks.noSearchResults') : t('ks.empty')}
          </div>
        )
      ) : (
        <>
          <div className="space-y-1">
            {sources.map((s) => (
              <CompactRow key={s.id} s={s} selected={selectedId === s.id} onSelect={() => onSelect(s.id)} />
            ))}
          </div>
          {usingPaged && <LoadMoreFooter loaded={sources.length} total={total} loading={paged.loadingMore} onLoadMore={paged.loadMore} />}
        </>
      )}
    </SectionFrame>
  );
}
