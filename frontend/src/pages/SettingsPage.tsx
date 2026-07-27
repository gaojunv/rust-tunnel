import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { PageHeader } from '@/components/layout/PageHeader';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import GeneralTab from '@/components/settings/GeneralTab';
import SecurityTab from '@/components/settings/SecurityTab';
import AppearanceTab from '@/components/settings/AppearanceTab';

export default function SettingsPage() {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState('general');

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('settings.title')}
        description={t('settings.description')}
      />

      <Tabs value={activeTab} onValueChange={setActiveTab}>
        <TabsList className="border bg-card/60 backdrop-blur-xl">
          <TabsTrigger
            value="general"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            {t('settings.tabs.general')}
          </TabsTrigger>
          <TabsTrigger
            value="security"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            {t('settings.tabs.security')}
          </TabsTrigger>
          <TabsTrigger
            value="appearance"
            className="data-[state=active]:bg-primary/10 data-[state=active]:text-primary"
          >
            {t('settings.tabs.appearance')}
          </TabsTrigger>
        </TabsList>

        <TabsContent value="general">
          <GeneralTab />
        </TabsContent>
        <TabsContent value="security">
          <SecurityTab />
        </TabsContent>
        <TabsContent value="appearance">
          <AppearanceTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
