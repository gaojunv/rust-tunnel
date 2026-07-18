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
        <TabsList className="border bg-card/60 backdrop-blur-xl">
          <TabsTrigger
            value="general"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            General
          </TabsTrigger>
          <TabsTrigger
            value="proxy"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            Proxy
          </TabsTrigger>
          <TabsTrigger
            value="dns"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            DNS
          </TabsTrigger>
          <TabsTrigger
            value="security"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            Security
          </TabsTrigger>
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
