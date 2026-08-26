import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { PageHeader } from '@/components/layout/PageHeader';
import { Button } from '@/components/ui/button';
import { Plus, ShieldOff } from 'lucide-react';
import { useAcmeCertificates, useAcmeStatus } from '@/api/hooks';
import { AcmeConfigCard } from '@/components/acme/AcmeConfigCard';
import { AcmeCertificateTable } from '@/components/acme/AcmeCertificateTable';
import { AcmeRequestDialog } from '@/components/acme/AcmeRequestDialog';
import { DnsProviderConfigCard } from '@/components/acme/DnsProviderConfigCard';
import { CertConsumerBinding } from '@/components/acme/CertConsumerBinding';

export default function AcmePage() {
  const { t } = useTranslation();
  const { data: certificates, isLoading: certsLoading } = useAcmeCertificates();
  const { data: status } = useAcmeStatus();
  const [requestOpen, setRequestOpen] = useState(false);

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('acme.title')}
        description={t('acme.description')}
      >
        <Button className="md:shadow-glow" onClick={() => setRequestOpen(true)} disabled={!status?.enabled}>
          <Plus className="mr-2 h-4 w-4" />
          {t('acme.requestCertificate')}
        </Button>
      </PageHeader>

      <div className="grid grid-cols-1 gap-6 md:grid-cols-2">
        <AcmeConfigCard />
        <DnsProviderConfigCard />
      </div>

      {!status?.enabled ? (
        <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
          <ShieldOff className="h-8 w-8 text-muted-foreground/50" />
          <p>{t('acme.notEnabled')}</p>
        </div>
      ) : (
        <AcmeCertificateTable
          certificates={certificates ?? []}
          isLoading={certsLoading}
        />
      )}

      <CertConsumerBinding consumers={status?.consumers} />

      <AcmeRequestDialog open={requestOpen} onOpenChange={setRequestOpen} />
    </div>
  );
}
