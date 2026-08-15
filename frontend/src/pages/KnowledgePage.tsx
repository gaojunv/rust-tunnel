import { useTranslation } from 'react-i18next';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { PageHeader } from '@/components/layout/PageHeader';
import SharedEmbeddingSettings from '@/components/knowledge/SharedEmbeddingSettings';
import KbSection from '@/components/knowledge/KbSection';
import MemorySection from '@/components/knowledge/MemorySection';

/** 知识库 + 会话记忆合并页：顶部共享 embedding 设置，下方两 Tab 切换。 */
export default function KnowledgePage() {
  const { t } = useTranslation();

  return (
    <div className="space-y-6">
      <PageHeader title={t('knowledge.title')} description={t('knowledge.description')} />
      <SharedEmbeddingSettings />
      <Tabs defaultValue="kb">
        <TabsList>
          <TabsTrigger value="kb">{t('nav.knowledgeBase')}</TabsTrigger>
          <TabsTrigger value="memory">{t('nav.memory')}</TabsTrigger>
        </TabsList>
        <TabsContent value="kb" className="mt-4">
          <KbSection />
        </TabsContent>
        <TabsContent value="memory" className="mt-4">
          <MemorySection />
        </TabsContent>
      </Tabs>
    </div>
  );
}
