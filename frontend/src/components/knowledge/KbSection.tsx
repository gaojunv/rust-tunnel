import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import KbList from '@/components/llm/kb/KbList';
import KbDialog from '@/components/llm/kb/KbDialog';
import KbDetail from '@/components/llm/kb/KbDetail';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import MasterDetail from '@/components/knowledge/shared/MasterDetail';
import { useLlmKbs } from '@/api/hooks';

export default function KbSection() {
  const { t } = useTranslation();
  const { data: kbs, isLoading } = useLlmKbs();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const selectedKb = kbs?.find((k) => k.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MasterDetail
        isLoading={isLoading}
        loadingText={t('common.loading')}
        hasSelection={!!selectedKb}
        emptyText={t('kb.noSelection')}
        list={
          <KbList
            kbs={kbs ?? []}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onNew={() => setDialogOpen(true)}
            onSettings={() => setSettingsOpen(true)}
          />
        }
        detail={
          selectedKb ? (
            <KbDetail
              key={selectedKb.id}
              kb={selectedKb}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : null
        }
      />
      <KbDialog open={dialogOpen} onClose={() => setDialogOpen(false)} kbId={null} onCreated={(id) => setSelectedId(id)} />
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('knowledge.sharedEmbeddingTitle')}</DialogTitle>
            <DialogDescription>{t('knowledge.sharedEmbeddingDesc')}</DialogDescription>
          </DialogHeader>
          <SharedEmbeddingSettings />
        </DialogContent>
      </Dialog>
    </div>
  );
}
