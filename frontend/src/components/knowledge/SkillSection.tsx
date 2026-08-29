import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import SkillList from '@/components/agent/skill/SkillList';
import SkillDetail from '@/components/agent/skill/SkillDetail';
import SkillDialog from '@/components/agent/skill/SkillDialog';
import SkillSettings from '@/components/knowledge/SkillSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { useSkills } from '@/api/hooks';
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

  const { data, isLoading } = useSkills(params);
  const skills = data?.skills ?? [];
  const selectedSkill = skills.find((s) => s.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={isLoading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedSkill}
        emptyText={t('skill.noSelection')}
        list={
          <SkillList
            skills={skills}
            filters={filters}
            onFiltersChange={setFilters}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
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
