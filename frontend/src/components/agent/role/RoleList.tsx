import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Plus, Pencil, Trash2 } from 'lucide-react';
import { useToggleRole, useDeleteRole } from '@/api/hooks';
import type { AgentRole, AgentRoleScope, RoleListParams } from '@/types';

interface Props {
  roles: AgentRole[];
  filters: RoleListParams;
  onFiltersChange: (filters: RoleListParams) => void;
  onNew: () => void;
  onEdit: (role: AgentRole) => void;
}

function scopeVariant(scope: AgentRoleScope): 'default' | 'secondary' | 'outline' {
  if (scope === 'global') return 'default';
  if (scope === 'client') return 'secondary';
  return 'outline';
}

function modeBadgeClass(mode: string): string {
  if (mode === 'subagent') return 'bg-teal-500/10 text-teal-600 dark:text-teal-400';
  if (mode === 'primary') return 'bg-violet-500/10 text-violet-600 dark:text-violet-400';
  return 'bg-slate-500/10 text-slate-600 dark:text-slate-400';
}

export default function RoleList({
  roles,
  filters,
  onFiltersChange,
  onNew,
  onEdit,
}: Props) {
  const { t } = useTranslation();
  const toggleMutation = useToggleRole();
  const deleteMutation = useDeleteRole();

  const [qInput, setQInput] = useState(filters.q ?? '');
  useEffect(() => {
    setQInput(filters.q ?? '');
  }, [filters.q]);
  useEffect(() => {
    const timer = setTimeout(() => {
      if (qInput !== (filters.q ?? '')) {
        onFiltersChange({ ...filters, q: qInput || undefined });
      }
    }, 300);
    return () => clearTimeout(timer);
  }, [qInput, filters, onFiltersChange]);

  const handleDelete = (role: AgentRole) => {
    if (!window.confirm(t('role.deleteConfirm', { name: role.name }))) return;
    deleteMutation.mutate(role.id);
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {t('role.listTitle')} ({roles.length})
        </h2>
        <Button size="sm" onClick={onNew}>
          <Plus className="mr-1 h-4 w-4" /> {t('role.newRole')}
        </Button>
      </div>

      <div className="flex items-center gap-2">
        <Input
          value={qInput}
          onChange={(e) => setQInput(e.target.value)}
          placeholder={t('role.searchPlaceholder')}
          aria-label={t('role.searchPlaceholder')}
          className="h-9 max-w-xs"
        />
        <div className="flex items-center gap-1.5 text-sm text-muted-foreground">
          <span>{t('role.enabledOnly')}</span>
          <Switch
            checked={filters.enabled === true}
            onCheckedChange={(v) =>
              onFiltersChange({ ...filters, enabled: v ? true : undefined })
            }
            aria-label={t('role.enabledOnly')}
          />
        </div>
      </div>

      {roles.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t('role.empty')}
          </CardContent>
        </Card>
      ) : (
        roles.map((r) => (
          <Card
            key={r.id}
            className="transition-colors hover:border-primary/30"
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <p className="truncate text-sm font-semibold">{r.name}</p>
                    {r.is_builtin && (
                      <Badge variant="secondary" className="text-[10px]">
                        {t('role.builtin')}
                      </Badge>
                    )}
                  </div>
                  {r.description && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">
                      {r.description}
                    </p>
                  )}
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 w-7 p-0"
                    onClick={() => onEdit(r)}
                    aria-label={t('role.editRole')}
                  >
                    <Pencil className="h-3.5 w-3.5" />
                  </Button>
                  {!r.is_builtin && (
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 w-7 p-0 text-destructive hover:text-destructive"
                      onClick={() => handleDelete(r)}
                      aria-label={t('role.deleteRole')}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  )}
                  <Switch
                    checked={r.enabled}
                    onCheckedChange={() => toggleMutation.mutate(r.id)}
                    aria-label={t('role.toggle')}
                    disabled={r.is_builtin}
                  />
                </div>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                <span
                  className={cn(
                    'inline-flex items-center gap-1',
                    r.enabled
                      ? 'text-emerald-600 dark:text-emerald-400'
                      : 'text-muted-foreground'
                  )}
                >
                  <span
                    className={cn(
                      'h-1.5 w-1.5 rounded-full',
                      r.enabled ? 'bg-emerald-500' : 'bg-muted-foreground/50'
                    )}
                  />
                  {r.enabled ? t('role.enabled') : t('role.disabled')}
                </span>
                <Badge variant={scopeVariant(r.scope_type)}>
                  {t(`role.scope_${r.scope_type}`)}
                </Badge>
                <span className={`rounded px-1.5 py-0.5 text-[10px] ${modeBadgeClass(r.mode)}`}>
                  {t(`role.mode_${r.mode}`)}
                </span>
                {r.model_override && (
                  <Badge variant="outline">{r.model_override}</Badge>
                )}
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}
