import { useState } from 'react';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { PageHeader } from '@/components/layout/PageHeader';
import GatewayTab from '@/components/llm/GatewayTab';
import ProviderCard from '@/components/llm/ProviderCard';
import ProviderDialog from '@/components/llm/ProviderDialog';
import ApiKeyTable from '@/components/llm/ApiKeyTable';
import UsageTab from '@/components/llm/UsageTab';
import { useLlmProviders } from '@/api/hooks';
import { Button } from '@/components/ui/button';
import { Plus } from 'lucide-react';

export default function LLMPage() {
  const [providerDialogOpen, setProviderDialogOpen] = useState(false);
  const [editingProvider, setEditingProvider] = useState<string | null>(null);
  const { data: providers, isLoading } = useLlmProviders();

  return (
    <div className="space-y-6">
      <PageHeader title="LLM Gateway" description="Manage AI providers, models, and API keys" />
      <Tabs defaultValue="usage">
        <TabsList>
          <TabsTrigger value="usage">Usage</TabsTrigger>
          <TabsTrigger value="gateway">Gateway</TabsTrigger>
          <TabsTrigger value="providers">Providers & Models</TabsTrigger>
          <TabsTrigger value="api-keys">API Keys</TabsTrigger>
        </TabsList>
        <TabsContent value="usage" className="mt-4"><UsageTab /></TabsContent>
        <TabsContent value="gateway" className="mt-4"><GatewayTab /></TabsContent>
        <TabsContent value="providers" className="mt-4 space-y-4">
          <div className="flex justify-between items-center">
            <h3 className="text-lg font-semibold">Providers</h3>
            <Button onClick={() => { setEditingProvider(null); setProviderDialogOpen(true); }}>
              <Plus className="w-4 h-4 mr-2" /> Add Provider
            </Button>
          </div>
          {isLoading ? <div className="text-muted-foreground">Loading...</div> : (
            providers?.map((p) => (
              <ProviderCard key={p.id} provider={p} onEdit={() => { setEditingProvider(p.id); setProviderDialogOpen(true); }} />
            ))
          )}
          <ProviderDialog open={providerDialogOpen} onClose={() => setProviderDialogOpen(false)} providerId={editingProvider} />
        </TabsContent>
        <TabsContent value="api-keys" className="mt-4"><ApiKeyTable /></TabsContent>
      </Tabs>
    </div>
  );
}
