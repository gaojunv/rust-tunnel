import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { AlertTriangle, Pencil, Trash2 } from 'lucide-react';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import { getApiErrorMessage } from '@/api/client';
import { useToggleRole, useDeleteRole } from '@/api/hooks';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import { useDebouncedSearch } from '@/components/knowledge/shared/useDebouncedSearch';
import LoadMoreFooter from '@/components/knowledge/shared/LoadMoreFooter';
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
  const confirmCtl = useConfirm();
  const [error, setError] = useState<string | null>(null);
  const [visibleCount, setVisibleCount] = useState(20);

  const [qInput, setQInput] = useDebouncedSearch(filters.q ?? '', (v) =>
    onFiltersChange({ ...filters, q: v || undefined }),
  );

  const filterActive = (filters.q ?? '').trim() !== '' || filters.enabled === true || (filters.scope !== undefined && filters.scope !== 'all');
  const emptyKey = filterActive ? 'role.noSearchResults' : 'role.empty';

  // 前端切片：API 不支持分页时在前端做 visibleCount 截断
  const visibleRoles = roles.slice(0, visibleCount);
  useEffect(() => {
    setVisibleCount(20);
  }, [filters.q, filters.enabled, filters.scope]);
  useEffect(() => {
    setError(null);
  }, [filters.q, filters.enabled, filters.scope]);

  const handleDelete = (role: AgentRole) => {
    confirmCtl.confirm(
      {
        title: t('role.deleteRole'),
        description: t('role.deleteConfirm', { name: role.name }),
        confirmLabel: t('common.delete'),
      },
      () =>
        deleteMutation.mutate(role.id, {
          onSuccess: () => toast.success(t('common.toast.deleted')),
          onError: (err) => setError(t('role.actionFailed', { error: getApiErrorMessage(err) })),
        }),
    );
  };

  return (
    <SectionFrame
      title={t('role.listTitle')}
      count={roles.length}
      newLabel={t('role.newRole')}
      onNew={onNew}
      settingsLabel=""
    >
      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={qInput}
          onChange={(e) => setQInput(e.target.value)}
          placeholder={t('role.searchPlaceholder')}
          aria-label={t('role.searchPlaceholder')}
          className="h-9 flex-1 min-w-[160px]"
        />
        <Select
          value={filters.scope ?? 'all'}
          onValueChange={(v) => onFiltersChange({ ...filters, scope: v })}
        >
          <SelectTrigger className="h-9 w-[148px]" aria-label={t('role.scopeFilter')}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">{t('role.scopeAll')}</SelectItem>
            <SelectItem value="global">{t('role.scope_global')}</SelectItem>
            <SelectItem value="client">{t('role.scope_client')}</SelectItem>
            <SelectItem value="workspace">{t('role.scope_workspace')}</SelectItem>
          </SelectContent>
        </Select>
        <div className="flex items-center gap-1.5 text-sm text-muted-foreground">
          <span>{t('role.enabledOnly')}</span>
          <Switch
            checked={filters.enabled === true}
            onCheckedChange={(v) => onFiltersChange({ ...filters, enabled: v ? true : undefined })}
            aria-label={t('role.enabledOnly')}
          />
        </div>
      </div>

      {error && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {error}
        </div>
      )}

      {roles.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t(emptyKey)}
          </CardContent>
        </Card>
      ) : (
        <>
          {visibleRoles.map((r) => (
            <Card key={r.id} className="transition-colors hover:border-primary/30">
              <CardContent className="flex h-12 items-center justify-between gap-3 px-3 py-0">
                <div className="flex min-w-0 flex-1 items-center gap-2">
                  <p className="truncate text-sm font-semibold">{r.name}</p>
                  {r.is_builtin && (
                    <Badge variant="secondary" className="shrink-0 text-[10px]">
                      {t('role.builtin')}
                    </Badge>
                  )}
                  <Badge variant={scopeVariant(r.scope_type)} className="shrink-0">
                    {t(`role.scope_${r.scope_type}`)}
                  </Badge>
                  <span className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] ${modeBadgeClass(r.mode)}`}>
                    {t(`role.mode_${r.mode}`)}
                  </span>
                  {r.model_override && (
                    <Badge variant="outline" className="hidden sm:inline-flex shrink-0">
                      {r.model_override}
                    </Badge>
                  )}
                </div>
                <div className="flex shrink-0 items-center gap-1">
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
                    onCheckedChange={() =>
                      toggleMutation.mutate(r.id, {
                        onError: (err) => setError(t('role.actionFailed', { error: getApiErrorMessage(err) })),
                      })
                    }
                    aria-label={t('role.toggle')}
                    disabled={r.is_builtin}
                  />
                </div>
              </CardContent>
            </Card>
          ))}
          <LoadMoreFooter
            loaded={Math.min(visibleCount, roles.length)}
            total={roles.length}
            onLoadMore={() => setVisibleCount((c) => c + 20)}
          />
        </>
      )}
      <ConfirmDialog
        open={confirmCtl.open}
        payload={confirmCtl.payload}
        onConfirm={confirmCtl.confirmAndClose}
        onCancel={confirmCtl.cancel}
        variant="destructive"
        confirmLabel={t('common.delete')}
        cancelLabel={t('common.cancel')}
      />
    </SectionFrame>
  );
}
