import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Pin } from 'lucide-react';
import { useMemoryStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import LoadMoreFooter from '@/components/knowledge/shared/LoadMoreFooter';
import { formatDateTime } from '@/utils/format';
import { useTranslation } from 'react-i18next';
import type { AgentMemory, AgentMemoryScope, MemoryFilters } from '@/types';

interface Props {
  memories: AgentMemory[];
  total?: number;
  filters: MemoryFilters;
  onFiltersChange: (filters: MemoryFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onSettings?: () => void;
  hasMore?: boolean;
  loadingMore?: boolean;
  onLoadMore?: () => void;
}

function scopeVariant(scope: AgentMemoryScope): 'default' | 'secondary' | 'outline' {
  if (scope === 'global') return 'default';
  if (scope === 'client') return 'secondary';
  return 'outline';
}

function hasActiveFilter(filters: MemoryFilters): boolean {
  return (
    (filters.q ?? '').trim() !== '' ||
    filters.scope !== 'all' ||
    filters.pinned ||
    !!filters.clientId ||
    !!filters.workspaceId
  );
}

export default function MemoryList({
  memories,
  total,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
  onSettings,
  hasMore,
  loadingMore,
  onLoadMore,
}: Props) {
  const { t } = useTranslation();
  useMemoryStream();

  const effectiveTotal = total ?? memories.length;
  const showLoadMore = hasMore !== undefined && onLoadMore !== undefined;

  return (
    <SectionFrame
      title={t('memory.listTitle')}
      count={effectiveTotal}
      newLabel={t('memory.newMemory')}
      onNew={onNew}
      onSettings={onSettings}
      settingsLabel={t('memory.settings.title')}
    >
      <ScopeFilterBar
        scope={filters.scope}
        clientId={filters.clientId}
        workspaceId={filters.workspaceId}
        q={filters.q}
        scopeLabelKey="memory.scopeLabel"
        searchPlaceholderKey="memory.searchPlaceholder"
        onScopeChange={(scope) => onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' })}
        onClientChange={(clientId) => onFiltersChange({ ...filters, clientId })}
        onWorkspaceChange={(workspaceId) => onFiltersChange({ ...filters, workspaceId })}
        onSearchChange={(q) => onFiltersChange({ ...filters, q })}
        extra={
          <div className="flex items-center justify-between">
            <span className="text-sm">{t('memory.pinnedOnly')}</span>
            <Switch
              checked={filters.pinned}
              onCheckedChange={(v) => onFiltersChange({ ...filters, pinned: v })}
              aria-label={t('memory.pinnedOnly')}
            />
          </div>
        }
      />

      {memories.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {hasActiveFilter(filters) ? t('memory.noSearchResults') : t('memory.empty')}
          </CardContent>
        </Card>
      ) : (
        <>
          {memories.map((m) => (
            <Card
              key={m.id}
              className={cn(
                'cursor-pointer transition-colors hover:border-primary/40',
                selectedId === m.id && 'border-primary/60 bg-primary/5',
              )}
              onClick={() => onSelect(m.id)}
            >
              <CardContent className="flex h-12 items-center justify-between gap-2 px-3 py-2">
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium leading-none" title={m.content}>
                    {m.content}
                  </p>
                  <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
                    {m.pinned && <Pin className="h-3 w-3 shrink-0 text-primary" />}
                    <Badge variant={scopeVariant(m.scope_type)} className="h-5 px-1.5 text-xs">
                      {t(`memory.scope_${m.scope_type}`)}
                    </Badge>
                    <Badge variant="outline" className="h-5 px-1.5 text-xs">
                      {t(`memory.trigger_${m.source_trigger}`)}
                    </Badge>
                    <span className="truncate text-xs">{formatDateTime(m.updated_at)}</span>
                  </div>
                </div>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {t('memory.hits', { count: m.hit_count })}
                </span>
              </CardContent>
            </Card>
          ))}
          {showLoadMore && (
            <LoadMoreFooter
              loaded={memories.length}
              total={effectiveTotal}
              loading={loadingMore}
              onLoadMore={onLoadMore}
            />
          )}
        </>
      )}
    </SectionFrame>
  );
}
