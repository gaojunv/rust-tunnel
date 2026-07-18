import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useDnsConfig, useUpdateDnsConfig } from '@/api/hooks';
import { Globe } from 'lucide-react';

export default function DnsTab() {
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
    return <div className="py-8 text-center text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Globe className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">DNS Configuration</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">Tunnel Domain</label>
            <Input
              type="text"
              value={tunnelDomain}
              onChange={(e) => setTunnelDomain(e.target.value)}
              placeholder="tunnel.local"
              className="max-w-md"
            />
            <p className="text-xs text-muted-foreground">
              Domain suffix used for tunnel address resolution
            </p>
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">Mesh Domain</label>
            <Input
              type="text"
              value={meshDomain}
              onChange={(e) => setMeshDomain(e.target.value)}
              placeholder="mesh.local"
              className="max-w-md"
            />
            <p className="text-xs text-muted-foreground">
              Domain suffix used for mesh network address resolution
            </p>
          </div>
          <Button onClick={handleSave} disabled={updateConfig.isPending}>
            {updateConfig.isPending ? 'Saving...' : 'Save'}
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
