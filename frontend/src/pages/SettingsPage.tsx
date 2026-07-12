import { useState } from 'react';
import { PageHeader } from '@/components/layout/PageHeader';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import GeneralTab from '@/components/settings/GeneralTab';
import ProxyTab from '@/components/settings/ProxyTab';
import DnsTab from '@/components/settings/DnsTab';
import SecurityTab from '@/components/settings/SecurityTab';

export default function SettingsPage() {
  const [activeTab, setActiveTab] = useState('general');

  return (
    <div className="space-y-6">
      <PageHeader
        title="Settings"
        description="Configure system settings, proxy services, DNS, and security"
      />

      <Tabs value={activeTab} onValueChange={setActiveTab}>
        <TabsList>
          <TabsTrigger value="general">General</TabsTrigger>
          <TabsTrigger value="proxy">Proxy</TabsTrigger>
          <TabsTrigger value="dns">DNS</TabsTrigger>
          <TabsTrigger value="security">Security</TabsTrigger>
        </TabsList>

        <TabsContent value="general">
          <GeneralTab />
        </TabsContent>
        <TabsContent value="proxy">
          <ProxyTab />
        </TabsContent>
        <TabsContent value="dns">
          <DnsTab />
        </TabsContent>
        <TabsContent value="security">
          <SecurityTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
