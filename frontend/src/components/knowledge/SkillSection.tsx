import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import SkillList from '@/components/agent/skill/SkillList';
import SkillDetail from '@/components/agent/skill/SkillDetail';
import SkillDialog from '@/components/agent/skill/SkillDialog';
import SkillSettings from '@/components/knowledge/SkillSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { usePagedList } from '@/components/knowledge/shared/usePagedList';
import { listSkills } from '@/api/client';
import type { AgentSkill, SkillFilters } from '@/types';

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
  const [settingsOpen, setSettingsOpen] = useState(false);

  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: (filters.q ?? '').trim() || undefined,
      enabled: filters.enabledOnly || undefined,
    }),
    [filters],
  );

  const fetchPage = useCallback(
    async (offset: number, limit: number) => {
      const res = await listSkills({ ...params, offset, limit });
      return { items: res.skills, total: res.total };
    },
    [params],
  );

  const filtersKey = useMemo(() => JSON.stringify(params), [params]);

  const { items: skills, total, loading, loadingMore, hasMore, loadMore } = usePagedList<AgentSkill>({
    fetchPage,
    filtersKey,
    pageSize: 20,
  });

  useEffect(() => {
    if (selectedId && skills.length > 0 && !skills.some((s) => s.id === selectedId)) {
      setSelectedId(null);
    }
    if (selectedId && skills.length === 0 && !loading) {
      setSelectedId(null);
    }
  }, [skills, selectedId, loading]);

  const selectedSkill = skills.find((s) => s.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={loading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedSkill}
        emptyText={t('skill.noSelection')}
        list={
          <SkillList
            skills={skills}
            total={total}
            filters={filters}
            onFiltersChange={setFilters}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
            hasMore={hasMore}
            loadingMore={loadingMore}
            onLoadMore={loadMore}
          />
        }
        detail={
          selectedSkill ? (
            <SkillDetail
              key={selectedSkill.id}
              skill={selectedSkill}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : null
        }
      />
      <SkillDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(s: AgentSkill) => setSelectedId(s.id)}
      />
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('skill.settings.title')}</DialogTitle>
            <DialogDescription>{t('skill.settings.enabledDesc')}</DialogDescription>
          </DialogHeader>
          <SkillSettings />
        </DialogContent>
      </Dialog>
    </div>
  );
}
