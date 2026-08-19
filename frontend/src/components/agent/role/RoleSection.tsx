import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import RoleList from './RoleList';
import RoleDialog from './RoleDialog';
import { useRoles } from '@/api/hooks';
import type { AgentRole, RoleListParams } from '@/types';

/** 角色库 Tab 内容。仿 SkillSection：列表 + 过滤状态 + 新建/编辑弹窗。 */
export default function RoleSection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<RoleListParams>({
    scope: 'all',
    q: '',
    enabled: undefined,
  });
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editingRole, setEditingRole] = useState<AgentRole | null>(null);

  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      q: (filters.q ?? '').trim() || undefined,
      enabled: filters.enabled,
    }),
    [filters],
  );

  const { data, isLoading } = useRoles(params);
  const roles = data?.roles ?? [];

  return (
    <div className="space-y-4">
      {isLoading ? (
        <Card>
          <CardContent className="p-6 text-sm text-muted-foreground">
            {t('common.loading')}
          </CardContent>
        </Card>
      ) : (
        <RoleList
          roles={roles}
          filters={filters}
          onFiltersChange={setFilters}
          onNew={() => {
            setEditingRole(null);
            setDialogOpen(true);
          }}
          onEdit={(role) => {
            setEditingRole(role);
            setDialogOpen(true);
          }}
        />
      )}
      <RoleDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        role={editingRole}
      />
    </div>
  );
}
