import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { useTranslation } from 'react-i18next';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { useWikiStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import type { AgentMemoryScope, AgentWiki } from '@/types';

/** Wiki 容器列表 UI 过滤条件（WikiSection 持有，映射到 API 查询参数）。 */
export interface WikiFilters {
  scope: 'all' | AgentMemoryScope;
  clientId: string;
  workspaceId: string;
  q: string;
  status: string;
}

interface Props {
  wikis: AgentWiki[];
  filters: WikiFilters;
  onFiltersChange: (filters: WikiFilters) => void;
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

export default function WikiList({
  wikis,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
  onSettings,
}: Props) {
  const { t } = useTranslation();
  useWikiStream();

  return (
    <SectionFrame
      title={t('wiki.listTitle')}
      count={wikis.length}
      newLabel={t('wiki.newWiki')}
      onNew={onNew}
      onSettings={onSettings}
      settingsLabel={t('wiki.settings.title')}
    >
      <ScopeFilterBar
        scope={filters.scope}
        clientId={filters.clientId}
        workspaceId={filters.workspaceId}
        q={filters.q}
        scopeLabelKey="wiki.scopeLabel"
        searchPlaceholderKey="wiki.searchPlaceholder"
        clientLabelKey="wiki.clientLabel"
        workspaceLabelKey="wiki.workspaceLabel"
        clientPlaceholderKey="wiki.clientPlaceholder"
        workspacePlaceholderKey="wiki.workspacePlaceholder"
        onScopeChange={(scope) => onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' })}
        onClientChange={(clientId) => onFiltersChange({ ...filters, clientId })}
        onWorkspaceChange={(workspaceId) => onFiltersChange({ ...filters, workspaceId })}
        onSearchChange={(q) => onFiltersChange({ ...filters, q })}
        extra={
          <Select
            value={filters.status || '__all__'}
            onValueChange={(v) => onFiltersChange({ ...filters, status: v === '__all__' ? '' : v })}
          >
            <SelectTrigger className="h-9" aria-label={t('wiki.statusFilter')}>
              <SelectValue placeholder={t('wiki.statusAll')} />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="__all__">{t('wiki.statusAll')}</SelectItem>
              <SelectItem value="draft">{t('wiki.status.draft')}</SelectItem>
              <SelectItem value="pending">{t('wiki.status.pending')}</SelectItem>
              <SelectItem value="processing">{t('wiki.status.processing')}</SelectItem>
              <SelectItem value="ready">{t('wiki.status.ready')}</SelectItem>
              <SelectItem value="failed">{t('wiki.status.failed')}</SelectItem>
            </SelectContent>
          </Select>
        }
      />

      {wikis.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t('wiki.empty')}
          </CardContent>
        </Card>
      ) : (
        wikis.map((w) => (
          <Card
            key={w.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === w.id && 'border-primary/60 bg-primary/5',
            )}
            onClick={() => onSelect(w.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-semibold">{w.name}</p>
                  {w.summary && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{w.summary}</p>
                  )}
                </div>
                <Badge variant={statusVariant(w.status)} className={cn('shrink-0', statusClass(w.status))}>
                  {t(`wiki.status.${w.status}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                <Badge variant="outline">{t(`wiki.scope_${w.scope_type}`)}</Badge>
                <span>{t('wiki.pageCount', { count: w.page_count })}</span>
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </SectionFrame>
  );
}
