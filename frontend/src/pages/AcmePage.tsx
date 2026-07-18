import { useState } from 'react';
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
  const { data: certificates, isLoading: certsLoading } = useAcmeCertificates();
  const { data: status } = useAcmeStatus();
  const [requestOpen, setRequestOpen] = useState(false);

  return (
    <div className="space-y-6">
      <PageHeader
        title="ACME Certificates"
        description="Manage automatic TLS certificates via ACME protocol"
      >
        <Button className="shadow-glow" onClick={() => setRequestOpen(true)} disabled={!status?.enabled}>
          <Plus className="mr-2 h-4 w-4" />
          Request Certificate
        </Button>
      </PageHeader>

      <div className="grid gap-6 md:grid-cols-2">
        <AcmeConfigCard />
        <DnsProviderConfigCard />
      </div>

      {!status?.enabled ? (
        <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
          <ShieldOff className="h-8 w-8 text-muted-foreground/50" />
          <p>ACME is not enabled. Please configure and enable ACME first.</p>
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
