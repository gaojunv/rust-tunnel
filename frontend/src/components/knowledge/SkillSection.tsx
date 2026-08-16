import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import SkillList from '@/components/agent/skill/SkillList';
import SkillDetail from '@/components/agent/skill/SkillDetail';
import SkillDialog from '@/components/agent/skill/SkillDialog';
import { useSkills } from '@/api/hooks';
import type { AgentSkill, SkillFilters } from '@/types';

/** 技能库 Tab 内容（Skill 二期）。仿 MemorySection：双栏 + 过滤状态。
 *  技能设置已收进页面右上角统一设置弹窗（KnowledgePage）。 */
export default function SkillSection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<SkillFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    enabledOnly: false,
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  // UI 过滤条件 → API 查询参数：空值剔除；scope 非 all 才发送；client/workspace
  // 仅对相应 scope 生效（切换 scope 时 SkillList 会清空这两项）。
  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: filters.q.trim() || undefined,
      enabled: filters.enabledOnly || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useSkills(params);
  const skills = data?.skills ?? [];
  const selectedSkill = skills.find((s) => s.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        {/* 移动端选中技能后隐藏列表，仅桌面保持左侧栏 */}
        <div className={selectedSkill ? 'hidden lg:block lg:w-80 lg:shrink-0' : 'lg:w-80 lg:shrink-0'}>
          {isLoading ? (
            <Card>
              <CardContent className="p-6 text-sm text-muted-foreground">
                {t('common.loading')}
              </CardContent>
            </Card>
          ) : (
            <SkillList
              skills={skills}
              filters={filters}
              onFiltersChange={setFilters}
              selectedId={selectedId}
              onSelect={setSelectedId}
              onNew={() => setDialogOpen(true)}
            />
          )}
        </div>
        <div className="min-w-0 flex-1">
          {selectedSkill ? (
            <SkillDetail
              key={selectedSkill.id}
              skill={selectedSkill}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : (
            <Card>
              <CardContent className="p-8 text-center text-sm text-muted-foreground">
                {t('skill.noSelection')}
              </CardContent>
            </Card>
          )}
        </div>
      </div>
      <SkillDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(s: AgentSkill) => setSelectedId(s.id)}
      />
    </div>
  );
}
