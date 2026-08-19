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
import MemorySettings from '@/components/agent/memory/MemorySettings';
import SkillSettings from '@/components/knowledge/SkillSettings';
import KbSection from '@/components/knowledge/KbSection';
import MemorySection from '@/components/knowledge/MemorySection';
import SkillSection from '@/components/knowledge/SkillSection';
import RoleSection from '@/components/agent/role/RoleSection';

/** 知识库 + 会话记忆合并页：Tab 行右侧「设置」按钮弹出统一设置弹窗
 *  （共享 Embedding / 记忆设置 / 技能设置 三个子 Tab）。 */
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
            <TabsTrigger value="skill">{t('nav.skill')}</TabsTrigger>
            <TabsTrigger value="roles">{t('nav.roles')}</TabsTrigger>
          </TabsList>
          <Button variant="outline" size="sm" onClick={() => setSettingsOpen(true)}>
            <Settings className="mr-1 h-4 w-4" />
            {t('nav.settings')}
          </Button>
        </div>
        <TabsContent value="kb" className="mt-4">
          <KbSection />
        </TabsContent>
        <TabsContent value="memory" className="mt-4">
          <MemorySection />
        </TabsContent>
        <TabsContent value="skill" className="mt-4">
          <SkillSection />
        </TabsContent>
        <TabsContent value="roles" className="mt-4">
          <RoleSection />
        </TabsContent>
      </Tabs>

      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('nav.settings')}</DialogTitle>
            <DialogDescription>{t('knowledge.settingsDesc')}</DialogDescription>
          </DialogHeader>
          <Tabs defaultValue="embedding">
            <TabsList>
              <TabsTrigger value="embedding">{t('knowledge.sharedEmbeddingTitle')}</TabsTrigger>
              <TabsTrigger value="memory">{t('nav.memory')}</TabsTrigger>
              <TabsTrigger value="skill">{t('nav.skill')}</TabsTrigger>
            </TabsList>
            <TabsContent value="embedding" className="mt-4">
              <SharedEmbeddingSettings />
            </TabsContent>
            <TabsContent value="memory" className="mt-4">
              <MemorySettings />
            </TabsContent>
            <TabsContent value="skill" className="mt-4">
              <SkillSettings />
            </TabsContent>
          </Tabs>
        </DialogContent>
      </Dialog>
    </div>
  );
}
