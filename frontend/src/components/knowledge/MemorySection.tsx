import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import MemoryList from '@/components/agent/memory/MemoryList';
import MemoryDetail from '@/components/agent/memory/MemoryDetail';
import MemoryDialog from '@/components/agent/memory/MemoryDialog';
import MemorySettings from '@/components/agent/memory/MemorySettings';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { usePagedList } from '@/components/knowledge/shared/usePagedList';
import { listMemories } from '@/api/client';
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
      q: (filters.q ?? '').trim() || undefined,
      pinned: filters.pinned || undefined,
    }),
    [filters],
  );

  const fetchPage = useCallback(
    async (offset: number, limit: number) => {
      const res = await listMemories({ ...params, offset, limit });
      return { items: res.memories, total: res.total };
    },
    [params],
  );

  const filtersKey = useMemo(() => JSON.stringify(params), [params]);

  const { items: memories, total, loading, loadingMore, hasMore, loadMore } = usePagedList<AgentMemory>({
    fetchPage,
    filtersKey,
    pageSize: 20,
  });

  // 搜索重置后，若选中项不在新结果中则清空选中；加载更多时保持选中不变。
  useEffect(() => {
    if (selectedId && memories.length > 0 && !memories.some((m) => m.id === selectedId)) {
      // 仅在非加载更多场景清空：hasMore 为 false 或 memories 刚重置时
      // 简化：只要选中项消失就清空（加载更多不会移除已有项，不会误清）
      setSelectedId(null);
    }
    if (selectedId && memories.length === 0 && !loading) {
      setSelectedId(null);
    }
  }, [memories, selectedId, loading]);

  const selectedMemory = memories.find((m) => m.id === selectedId) ?? null;

  const handleCreated = (m: AgentMemory) => {
    setSelectedId(m.id);
  };

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={loading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedMemory}
        emptyText={t('memory.noSelection')}
        list={
          <MemoryList
            memories={memories}
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
        onCreated={handleCreated}
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
