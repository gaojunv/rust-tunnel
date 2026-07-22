import { useEffect, useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { useLlmGatewayConfig, useUpdateLlmGatewayConfig } from '@/api/hooks';

export default function GatewayTab() {
  const { data: config, isLoading } = useLlmGatewayConfig();
  const updateMutation = useUpdateLlmGatewayConfig();
  const [enabled, setEnabled] = useState(false);
  const [domain, setDomain] = useState('');
  const [listen, setListen] = useState('0.0.0.0:443');
  const [tlsEnabled, setTlsEnabled] = useState(true);
  const [tlsAcme, setTlsAcme] = useState(false);

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled);
      setDomain(config.domain || '');
      setListen(config.listen || '0.0.0.0:443');
      setTlsEnabled(config.tls_enabled ?? true);
      setTlsAcme(config.tls_acme ?? false);
    }
  }, [config]);

  if (isLoading) return <div className="text-muted-foreground">Loading...</div>;

  return (
    <Card>
      <CardHeader><CardTitle>Gateway Configuration</CardTitle></CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between">
          <Label>Enable Gateway</Label>
          <Switch checked={enabled} onCheckedChange={setEnabled} />
        </div>
        <div className="space-y-2">
          <Label>Domain</Label>
          <Input value={domain} onChange={(e) => setDomain(e.target.value)} placeholder="llm.example.com" disabled={!enabled} />
        </div>
        <div className="space-y-2">
          <Label>Listen Address</Label>
          <Input value={listen} onChange={(e) => setListen(e.target.value)} placeholder="0.0.0.0:443" disabled={!enabled} />
        </div>
        <div className="flex items-center justify-between">
          <Label>TLS</Label>
          <Switch checked={tlsEnabled} onCheckedChange={setTlsEnabled} disabled={!enabled} />
        </div>
        {tlsEnabled && (
          <div className="flex items-center justify-between">
            <Label>ACME Auto-Renew</Label>
            <Switch checked={tlsAcme} onCheckedChange={setTlsAcme} disabled={!enabled} />
          </div>
        )}
        <Button onClick={() => updateMutation.mutate({ enabled, domain, listen, tls_enabled: tlsEnabled, tls_acme: tlsAcme })} disabled={updateMutation.isPending}>
          {updateMutation.isPending ? 'Saving...' : 'Save'}
        </Button>
      </CardContent>
    </Card>
  );
}
