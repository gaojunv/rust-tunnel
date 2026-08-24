import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import MemoryList from '@/components/agent/memory/MemoryList';
import MemoryDetail from '@/components/agent/memory/MemoryDetail';
import MemoryDialog from '@/components/agent/memory/MemoryDialog';
import MemorySettings from '@/components/agent/memory/MemorySettings';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { useMemories } from '@/api/hooks';
import type { AgentMemory, MemoryFilters } from '@/types';

export default function MemorySection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<MemoryFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    pinned: false,
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
      pinned: filters.pinned || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useMemories(params);
  const memories = data?.memories ?? [];
  const selectedMemory = memories.find((m) => m.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={isLoading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedMemory}
        emptyText={t('memory.noSelection')}
        list={
          <MemoryList
            memories={memories}
            filters={filters}
            onFiltersChange={setFilters}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
          />
        }
        detail={
          selectedMemory ? (
            <MemoryDetail
              key={selectedMemory.id}
              memory={selectedMemory}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : null
        }
      />
      <MemoryDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(m: AgentMemory) => setSelectedId(m.id)}
      />
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('memory.settings.title')}</DialogTitle>
            <DialogDescription>{t('knowledge.sharedEmbeddingDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-6">
            <SharedEmbeddingSettings />
            <MemorySettings />
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
