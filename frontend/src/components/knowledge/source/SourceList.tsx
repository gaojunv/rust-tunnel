import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Database, FileStack } from 'lucide-react';
import { useKnowledgeStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
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
  sources: KnowledgeSource[];
  filters: SourceFilters;
  onFiltersChange: (filters: SourceFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onSettings?: () => void;
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

export default function SourceList({
  sources,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
  onSettings,
}: Props) {
  const { t } = useTranslation();
  useKnowledgeStream();

  const searching = (filters.q ?? '').trim().length > 0;

  return (
    <SectionFrame
      title={t('ks.listTitle')}
      count={sources.length}
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
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {searching ? t('ks.noSearchResults') : t('ks.empty')}
          </CardContent>
        </Card>
      ) : (
        sources.map((s) => (
          <Card
            key={s.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === s.id && 'border-primary/60 bg-primary/5',
              // 总闸关闭的容器整卡降饱和，与状态徽章区分：状态说的是索引进度，这里说的是是否生效
              !s.enabled && 'opacity-60',
            )}
            onClick={() => onSelect(s.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-semibold">{s.name}</p>
                  {s.summary && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{s.summary}</p>
                  )}
                </div>
                <Badge variant={statusVariant(s.status)} className={cn('shrink-0', statusClass(s.status))}>
                  {t(`ks.status.${s.status}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                {s.index_vector && (
                  <Badge variant="outline" className="gap-1 border-sky-500/40 text-sky-600 dark:text-sky-400">
                    <Database className="h-3 w-3" />
                    {t('ks.badgeVector')}
                  </Badge>
                )}
                {s.index_pages && (
                  <Badge variant="outline" className="gap-1 border-violet-500/40 text-violet-600 dark:text-violet-400">
                    <FileStack className="h-3 w-3" />
                    {t('ks.badgePages')}
                  </Badge>
                )}
                <Badge variant="outline">{t(`ks.scope_${s.scope_type}`)}</Badge>
                {!s.enabled && <Badge variant="secondary">{t('ks.disabled')}</Badge>}
              </div>
              <div className="mt-1.5 flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
                <span>{t('ks.docCount', { count: s.doc_count })}</span>
                {s.index_pages && (
                  <>
                    <span aria-hidden>·</span>
                    <span>{t('ks.pageCount', { count: s.page_count })}</span>
                  </>
                )}
                {s.index_vector && s.emb_model && (
                  <>
                    <span aria-hidden>·</span>
                    <span className="truncate">{s.emb_model}</span>
                  </>
                )}
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </SectionFrame>
  );
}
