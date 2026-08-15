import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Plus, Search } from 'lucide-react';
import { useAgentWorkspaces, useClients, useMemoryStream } from '@/api/hooks';
import type { AgentMemoryScope, AgentSkill, SkillFilters } from '@/types';

interface Props {
  skills: AgentSkill[];
  filters: SkillFilters;
  onFiltersChange: (filters: SkillFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
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
}: Props) {
  const { t } = useTranslation();
  // 技能无独立 SSE 流：复用记忆 SSE（事件体含 skills_found），到达即失效列表。
  useMemoryStream();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  // 搜索框本地输入 + 300ms 防抖提交到 filters（避免每次按键触发请求）。
  const [qInput, setQInput] = useState(filters.q);
  useEffect(() => {
    setQInput(filters.q);
  }, [filters.q]);
  useEffect(() => {
    const timer = setTimeout(() => {
      if (qInput !== filters.q) {
        onFiltersChange({ ...filters, q: qInput });
      }
    }, 300);
    return () => clearTimeout(timer);
  }, [qInput, filters, onFiltersChange]);

  const changeScope = (scope: SkillFilters['scope']) => {
    onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' });
  };

  const selectClass =
    'h-9 w-full rounded-md border border-input bg-background px-2 py-1 text-sm disabled:cursor-not-allowed disabled:opacity-50';

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {t('skill.listTitle')} ({skills.length})
        </h2>
        <Button size="sm" onClick={onNew}>
          <Plus className="mr-1 h-4 w-4" /> {t('skill.newSkill')}
        </Button>
      </div>

      {/* 过滤栏：作用域 / 客户端 / 工作区 / 搜索 / 仅启用 */}
      <Card>
        <CardContent className="space-y-2 p-3">
          <div className="flex items-center gap-2">
            <select
              aria-label={t('skill.scopeLabel')}
              value={filters.scope}
              onChange={(e) => changeScope(e.target.value as SkillFilters['scope'])}
              className="h-9 w-28 shrink-0 rounded-md border border-input bg-background px-2 py-1 text-sm"
            >
              <option value="all">{t('skill.all')}</option>
              <option value="global">{t('skill.scope_global')}</option>
              <option value="client">{t('skill.scope_client')}</option>
              <option value="workspace">{t('skill.scope_workspace')}</option>
            </select>
            <div className="relative flex-1">
              <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={qInput}
                onChange={(e) => setQInput(e.target.value)}
                placeholder={t('skill.searchPlaceholder')}
                aria-label={t('skill.searchPlaceholder')}
                className="h-9 pl-8"
              />
            </div>
          </div>
          {filters.scope === 'client' && (
            <select
              aria-label={t('skill.clientLabel')}
              value={filters.clientId}
              onChange={(e) => onFiltersChange({ ...filters, clientId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('skill.clientPlaceholder')}</option>
              {(clients ?? []).map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </select>
          )}
          {filters.scope === 'workspace' && (
            <select
              aria-label={t('skill.workspaceLabel')}
              value={filters.workspaceId}
              onChange={(e) => onFiltersChange({ ...filters, workspaceId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('skill.workspacePlaceholder')}</option>
              {(workspaces ?? []).map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          )}
          <div className="flex items-center justify-between">
            <span className="text-sm">{t('skill.enabledOnly')}</span>
            <Switch
              checked={filters.enabledOnly}
              onCheckedChange={(v) => onFiltersChange({ ...filters, enabledOnly: v })}
              aria-label={t('skill.enabledOnly')}
            />
          </div>
        </CardContent>
      </Card>

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
              selectedId === s.id && 'border-primary/60 bg-primary/5'
            )}
            onClick={() => onSelect(s.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-semibold">{s.name}</p>
                  {s.description && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">
                      {s.description}
                    </p>
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
                    s.enabled ? 'text-emerald-600 dark:text-emerald-400' : 'text-muted-foreground'
                  )}
                >
                  <span
                    className={cn(
                      'h-1.5 w-1.5 rounded-full',
                      s.enabled ? 'bg-emerald-500' : 'bg-muted-foreground/50'
                    )}
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
    </div>
  );
}
