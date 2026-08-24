import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { useMemoryStream } from '@/api/hooks';
import ScopeFilterBar from '@/components/knowledge/shared/ScopeFilterBar';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import { useTranslation } from 'react-i18next';
import type { AgentMemoryScope, AgentSkill, SkillFilters } from '@/types';

interface Props {
  skills: AgentSkill[];
  filters: SkillFilters;
  onFiltersChange: (filters: SkillFilters) => void;
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

export default function SkillList({
  skills,
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
      title={t('skill.listTitle')}
      count={skills.length}
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
            {t('skill.empty')}
          </CardContent>
        </Card>
      ) : (
        skills.map((s) => (
          <Card
            key={s.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === s.id && 'border-primary/60 bg-primary/5',
            )}
            onClick={() => onSelect(s.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-semibold">{s.name}</p>
                  {s.description && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{s.description}</p>
                  )}
                </div>
                <Badge variant={scopeVariant(s.scope_type)} className="shrink-0">
                  {t(`skill.scope_${s.scope_type}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
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
                <Badge variant="outline">{t(`skill.trigger_${s.source_trigger}`)}</Badge>
                {s.tags.slice(0, 3).map((tag) => (
                  <Badge key={tag} variant="secondary">
                    {tag}
                  </Badge>
                ))}
                <span className="ml-auto">{t('skill.uses', { count: s.use_count })}</span>
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </SectionFrame>
  );
}
