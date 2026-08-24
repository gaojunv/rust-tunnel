import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import WikiList, { type WikiFilters } from '@/components/knowledge/wiki/WikiList';
import WikiDetail from '@/components/knowledge/wiki/WikiDetail';
import WikiDialog from '@/components/knowledge/wiki/WikiDialog';
import WikiSettings from '@/components/knowledge/WikiSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { useWikis } from '@/api/hooks';
import type { AgentWiki } from '@/types';

export default function WikiSection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<WikiFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    status: '',
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: filters.q.trim() || undefined,
      status: filters.status || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useWikis(params);
  const wikis = data?.wikis ?? [];
  const selectedWiki = wikis.find((w) => w.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={isLoading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedWiki}
        emptyText={t('wiki.noSelection')}
        list={
          <WikiList
            wikis={wikis}
            filters={filters}
            onFiltersChange={setFilters}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
          />
        }
        detail={
          selectedWiki ? (
            <WikiDetail
              key={selectedWiki.id}
              wiki={selectedWiki}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : null
        }
      />
      <WikiDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(w: AgentWiki) => setSelectedId(w.id)}
      />
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('wiki.settings.title')}</DialogTitle>
            <DialogDescription>{t('wiki.settings.enabledDesc')}</DialogDescription>
          </DialogHeader>
          <WikiSettings />
        </DialogContent>
      </Dialog>
    </div>
  );
}
