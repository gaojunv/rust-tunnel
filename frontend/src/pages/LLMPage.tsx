import { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Skeleton } from '@/components/ui/skeleton';
import { Card, CardContent } from '@/components/ui/card';
import { PageHeader } from '@/components/layout/PageHeader';
import GatewayTab from '@/components/llm/GatewayTab';
import ProviderCard from '@/components/llm/ProviderCard';
import ProviderDialog from '@/components/llm/ProviderDialog';
import ApiKeyTable from '@/components/llm/ApiKeyTable';
import UsageTab from '@/components/llm/UsageTab';
import { GroupsTab } from '@/components/llm/groups/GroupsTab';
import { useLlmProviders } from '@/api/hooks';
import { Button } from '@/components/ui/button';
import { Plus, Server } from 'lucide-react';

const TAB_VALUES = ['usage', 'gateway', 'providers', 'api-keys', 'groups'] as const;
type LlmTab = (typeof TAB_VALUES)[number];
const DEFAULT_TAB: LlmTab = 'usage';

export default function LLMPage() {
  const { t } = useTranslation();
  const [searchParams, setSearchParams] = useSearchParams();
  const raw = searchParams.get('tab');
  const activeTab: LlmTab = (TAB_VALUES as readonly string[]).includes(raw ?? '')
    ? (raw as LlmTab)
    : DEFAULT_TAB;
  const setActiveTab = useCallback(
    (v: string) => {
      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set('tab', v);
        return next;
      }, { replace: true });
    },
    [setSearchParams],
  );
  const [providerDialogOpen, setProviderDialogOpen] = useState(false);
  const [editingProvider, setEditingProvider] = useState<string | null>(null);
  const { data: providers, isLoading } = useLlmProviders();

  return (
    <div className="space-y-6">
      <PageHeader title={t('llm.title')} description={t('llm.description')} />
      <Tabs value={activeTab} onValueChange={setActiveTab}>
        {/* 5 个 tab 在窄屏会挤压：允许横向滚动但隐藏滚动条（cn() 的 tailwind-merge 会误去重，错开修饰符）。 */}
        <TabsList className="w-full justify-start overflow-x-auto">
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
          {isLoading ? (
            <div className="space-y-3">
              <Skeleton className="h-24 w-full rounded-lg" />
              <Skeleton className="h-24 w-full rounded-lg" />
            </div>
          ) : !providers || providers.length === 0 ? (
            <Card>
              <CardContent className="flex flex-col items-center gap-2 py-10 text-muted-foreground">
                <Server className="h-8 w-8" />
                <div>{t('llm.providersEmpty')}</div>
              </CardContent>
            </Card>
          ) : (
            providers.map((p) => (
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
