import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { PageHeader } from '@/components/layout/PageHeader';
import GatewayTab from '@/components/llm/GatewayTab';
import ProviderCard from '@/components/llm/ProviderCard';
import ProviderDialog from '@/components/llm/ProviderDialog';
import ApiKeyTable from '@/components/llm/ApiKeyTable';
import UsageTab from '@/components/llm/UsageTab';
import { GroupsTab } from '@/components/llm/groups/GroupsTab';
import { useLlmProviders } from '@/api/hooks';
import { Button } from '@/components/ui/button';
import { Plus } from 'lucide-react';

export default function LLMPage() {
  const { t } = useTranslation();
  const [providerDialogOpen, setProviderDialogOpen] = useState(false);
  const [editingProvider, setEditingProvider] = useState<string | null>(null);
  const { data: providers, isLoading } = useLlmProviders();

  return (
    <div className="space-y-6">
      <PageHeader title={t('llm.title')} description={t('llm.description')} />
      <Tabs defaultValue="usage">
        <TabsList>
          <TabsTrigger value="usage">{t('llm.tabs.usage')}</TabsTrigger>
          <TabsTrigger value="gateway">{t('llm.tabs.gateway')}</TabsTrigger>
          <TabsTrigger value="providers">{t('llm.tabs.providers')}</TabsTrigger>
          <TabsTrigger value="api-keys">{t('llm.tabs.apiKeys')}</TabsTrigger>
          <TabsTrigger value="groups">{t('llm.tabs.groups')}</TabsTrigger>
        </TabsList>
        <TabsContent value="usage" className="mt-4"><UsageTab /></TabsContent>
        <TabsContent value="gateway" className="mt-4"><GatewayTab /></TabsContent>
        <TabsContent value="providers" className="mt-4 space-y-4">
          <div className="flex justify-between items-center">
            <h3 className="text-lg font-semibold">{t('llm.providers')}</h3>
            <Button onClick={() => { setEditingProvider(null); setProviderDialogOpen(true); }}>
              <Plus className="w-4 h-4 mr-2" /> {t('llm.addProvider')}
            </Button>
          </div>
          {isLoading ? <div className="text-muted-foreground">{t('common.loading')}</div> : (
            providers?.map((p) => (
              <ProviderCard key={p.id} provider={p} onEdit={() => { setEditingProvider(p.id); setProviderDialogOpen(true); }} />
            ))
          )}
          <ProviderDialog open={providerDialogOpen} onClose={() => setProviderDialogOpen(false)} providerId={editingProvider} />
        </TabsContent>
        <TabsContent value="api-keys" className="mt-4"><ApiKeyTable /></TabsContent>
        <TabsContent value="groups" className="mt-4">
          <GroupsTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
