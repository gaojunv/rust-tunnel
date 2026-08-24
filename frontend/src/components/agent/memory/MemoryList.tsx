import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Pin } from 'lucide-react';
import { useMemoryStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import { useTranslation } from 'react-i18next';
import type { AgentMemory, AgentMemoryScope, MemoryFilters } from '@/types';

interface Props {
  memories: AgentMemory[];
  filters: MemoryFilters;
  onFiltersChange: (filters: MemoryFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onSettings?: () => void;
}

function scopeVariant(scope: AgentMemoryScope): 'default' | 'secondary' | 'outline' {
  if (scope === 'global') return 'default';
  if (scope === 'client') return 'secondary';
  return 'outline';
}

export default function MemoryList({
  memories,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
  onSettings,
}: Props) {
  const { t } = useTranslation();
  useMemoryStream();

  return (
    <SectionFrame
      title={t('memory.listTitle')}
      count={memories.length}
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
            {t('memory.empty')}
          </CardContent>
        </Card>
      ) : (
        memories.map((m) => (
          <Card
            key={m.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === m.id && 'border-primary/60 bg-primary/5',
            )}
            onClick={() => onSelect(m.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <p className="line-clamp-2 text-sm font-medium">{m.content}</p>
                <Badge variant={scopeVariant(m.scope_type)} className="shrink-0">
                  {t(`memory.scope_${m.scope_type}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                {m.pinned && (
                  <span className="inline-flex items-center gap-0.5 text-primary">
                    <Pin className="h-3 w-3" />
                    {t('memory.pinned')}
                  </span>
                )}
                <Badge variant="outline">{t(`memory.trigger_${m.source_trigger}`)}</Badge>
                {m.tags.slice(0, 3).map((tag) => (
                  <Badge key={tag} variant="secondary">
                    {tag}
                  </Badge>
                ))}
                <span className="ml-auto">{t('memory.hits', { count: m.hit_count })}</span>
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </SectionFrame>
  );
}
