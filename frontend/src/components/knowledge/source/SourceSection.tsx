import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import SourceList, { EMPTY_SOURCE_FILTERS, type SourceFilters } from '@/components/knowledge/source/SourceList';
import SourceDetail from '@/components/knowledge/source/SourceDetail';
import SourceDialog from '@/components/knowledge/source/SourceDialog';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import WikiSettings from '@/components/knowledge/WikiSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { useKnowledgeSources } from '@/api/hooks';
import type { KnowledgeSource } from '@/types';

/** 统一知识容器分区：取代原 KbSection（向量库）与 WikiSection（Wiki 容器）。 */
export default function SourceSection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<SourceFilters>(EMPTY_SOURCE_FILTERS);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const params = useMemo(
    () => ({
      index_kind: filters.indexKind === 'all' ? undefined : filters.indexKind,
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: (filters.q ?? '').trim() || undefined,
      status: filters.status || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useKnowledgeSources(params);
  const sources = data?.sources ?? [];
  const selected = sources.find((s) => s.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={isLoading}
        loadingText={t('common.loading')}
        hasSelection={!!selected}
        emptyText={t('ks.noSelection')}
        list={
          <SourceList
            sources={sources}
            filters={filters}
            onFiltersChange={setFilters}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
          />
        }
        detail={
          selected ? (
            <SourceDetail
              key={selected.id}
              source={selected}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : null
        }
      />
      <SourceDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(s: KnowledgeSource) => setSelectedId(s.id)}
      />
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('ks.settingsTitle')}</DialogTitle>
            <DialogDescription>{t('ks.settingsDesc')}</DialogDescription>
          </DialogHeader>
          {/* 两块全局设置：向量侧的共享 embedding 回退，与 pages 侧的 agent wiki 工具开关。
              二者都不属于单个容器，故合并进同一个设置弹窗。 */}
          <section className="space-y-4">
            <div>
              <h3 className="text-base font-semibold">{t('knowledge.sharedEmbeddingTitle')}</h3>
              <p className="mt-1 text-sm text-muted-foreground">{t('knowledge.sharedEmbeddingDesc')}</p>
            </div>
            <SharedEmbeddingSettings />
          </section>
          <section className="border-t pt-4">
            <WikiSettings />
          </section>
        </DialogContent>
      </Dialog>
    </div>
  );
}
