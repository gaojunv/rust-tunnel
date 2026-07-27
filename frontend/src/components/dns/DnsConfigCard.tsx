import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useDnsConfig, useUpdateDnsConfig } from '@/api/hooks';
import { Globe } from 'lucide-react';

export default function DnsConfigCard() {
  const { t } = useTranslation();
  const { data, isLoading } = useDnsConfig();
  const updateConfig = useUpdateDnsConfig();

  const [tunnelDomain, setTunnelDomain] = useState('tunnel.local');
  const [meshDomain, setMeshDomain] = useState('mesh.local');

  useEffect(() => {
    if (data) {
      setTunnelDomain(data.tunnel_domain || 'tunnel.local');
      setMeshDomain(data.mesh_domain || 'mesh.local');
    }
  }, [data]);

  const handleSave = () => {
    updateConfig.mutate({
      tunnel_domain: tunnelDomain,
      mesh_domain: meshDomain,
    });
  };

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">{t('common.loading')}</div>;
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-3">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
            <Globe className="h-4 w-4" />
          </div>
          <CardTitle className="text-lg">{t('dns.config.title')}</CardTitle>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-2">
          <label className="text-sm font-medium">{t('dns.config.tunnelDomain')}</label>
          <Input
            type="text"
            value={tunnelDomain}
            onChange={(e) => setTunnelDomain(e.target.value)}
            placeholder="tunnel.local"
            className="max-w-md"
          />
          <p className="text-xs text-muted-foreground">
            {t('dns.config.tunnelDomainHint')}
          </p>
        </div>
        <div className="space-y-2">
          <label className="text-sm font-medium">{t('dns.config.meshDomain')}</label>
          <Input
            type="text"
            value={meshDomain}
            onChange={(e) => setMeshDomain(e.target.value)}
            placeholder="mesh.local"
            className="max-w-md"
          />
          <p className="text-xs text-muted-foreground">
            {t('dns.config.meshDomainHint')}
          </p>
        </div>
        <Button onClick={handleSave} disabled={updateConfig.isPending}>
          {updateConfig.isPending ? t('common.saving') : t('common.save')}
        </Button>
      </CardContent>
    </Card>
  );
}
