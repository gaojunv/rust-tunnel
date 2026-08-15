import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Settings } from 'lucide-react';
import { PageHeader } from '@/components/layout/PageHeader';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import KbSection from '@/components/knowledge/KbSection';
import MemorySection from '@/components/knowledge/MemorySection';

/** 知识库 + 会话记忆合并页：Tab 行右侧「设置」按钮弹出共享 Embedding 配置。 */
export default function KnowledgePage() {
  const { t } = useTranslation();
  const [settingsOpen, setSettingsOpen] = useState(false);

  return (
    <div className="space-y-6">
      <PageHeader title={t('knowledge.title')} description={t('knowledge.description')} />
      <Tabs defaultValue="kb">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <TabsList>
            <TabsTrigger value="kb">{t('nav.knowledgeBase')}</TabsTrigger>
            <TabsTrigger value="memory">{t('nav.memory')}</TabsTrigger>
          </TabsList>
          <Button variant="outline" size="sm" onClick={() => setSettingsOpen(true)}>
            <Settings className="mr-1 h-4 w-4" />
            {t('knowledge.sharedEmbeddingTitle')}
          </Button>
        </div>
        <TabsContent value="kb" className="mt-4">
          <KbSection />
        </TabsContent>
        <TabsContent value="memory" className="mt-4">
          <MemorySection />
        </TabsContent>
      </Tabs>

      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto">
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
