import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { useMemoryStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import LoadMoreFooter from '@/components/knowledge/shared/LoadMoreFooter';
import { formatDateTime } from '@/utils/format';
import { useTranslation } from 'react-i18next';
import type { AgentMemoryScope, AgentSkill, SkillFilters } from '@/types';

interface Props {
  skills: AgentSkill[];
  total?: number;
  filters: SkillFilters;
  onFiltersChange: (filters: SkillFilters) => void;
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

function hasActiveFilter(filters: SkillFilters): boolean {
  return (
    (filters.q ?? '').trim() !== '' ||
    filters.scope !== 'all' ||
    filters.enabledOnly ||
    !!filters.clientId ||
    !!filters.workspaceId
  );
}

export default function SkillList({
  skills,
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

  const effectiveTotal = total ?? skills.length;
  const showLoadMore = hasMore !== undefined && onLoadMore !== undefined;

  return (
    <SectionFrame
      title={t('skill.listTitle')}
      count={effectiveTotal}
      newLabel={t('skill.newSkill')}
      onNew={onNew}
      onSettings={onSettings}
      settingsLabel={t('skill.settings.title')}
    >
      <ScopeFilterBar
        scope={filters.scope}
        clientId={filters.clientId}
        workspaceId={filters.workspaceId}
        q={filters.q}
        scopeLabelKey="skill.scopeLabel"
        searchPlaceholderKey="skill.searchPlaceholder"
        clientLabelKey="skill.clientLabel"
        workspaceLabelKey="skill.workspaceLabel"
        clientPlaceholderKey="skill.clientPlaceholder"
        workspacePlaceholderKey="skill.workspacePlaceholder"
        onScopeChange={(scope) => onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' })}
        onClientChange={(clientId) => onFiltersChange({ ...filters, clientId })}
        onWorkspaceChange={(workspaceId) => onFiltersChange({ ...filters, workspaceId })}
        onSearchChange={(q) => onFiltersChange({ ...filters, q })}
        extra={
          <div className="flex items-center justify-between">
            <span className="text-sm">{t('skill.enabledOnly')}</span>
            <Switch
              checked={filters.enabledOnly}
              onCheckedChange={(v) => onFiltersChange({ ...filters, enabledOnly: v })}
              aria-label={t('skill.enabledOnly')}
            />
          </div>
        }
      />

      {skills.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {hasActiveFilter(filters) ? t('skill.noSearchResults') : t('skill.empty')}
          </CardContent>
        </Card>
      ) : (
        <>
          {skills.map((s) => (
            <Card
              key={s.id}
              className={cn(
                'cursor-pointer transition-colors hover:border-primary/40',
                selectedId === s.id && 'border-primary/60 bg-primary/5',
              )}
              onClick={() => onSelect(s.id)}
            >
              <CardContent className="flex h-12 items-center justify-between gap-2 px-3 py-2">
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-semibold leading-none" title={s.name}>
                    {s.name}
                  </p>
                  <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
                    <span
                      className={cn(
                        'inline-flex items-center gap-1',
                        s.enabled ? 'text-emerald-600 dark:text-emerald-400' : 'text-muted-foreground',
                      )}
                    >
                      <span
                        className={cn('h-1.5 w-1.5 rounded-full', s.enabled ? 'bg-emerald-500' : 'bg-muted-foreground/50')}
                      />
                      {s.enabled ? t('skill.enabled') : t('skill.disabled')}
                    </span>
                    <Badge variant={scopeVariant(s.scope_type)} className="h-5 px-1.5 text-xs">
                      {t(`skill.scope_${s.scope_type}`)}
                    </Badge>
                    <Badge variant="outline" className="h-5 px-1.5 text-xs">
                      {t(`skill.trigger_${s.source_trigger}`)}
                    </Badge>
                    <span className="truncate text-xs">{formatDateTime(s.updated_at)}</span>
                  </div>
                </div>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {t('skill.uses', { count: s.use_count })}
                </span>
              </CardContent>
            </Card>
          ))}
          {showLoadMore && (
            <LoadMoreFooter
              loaded={skills.length}
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
