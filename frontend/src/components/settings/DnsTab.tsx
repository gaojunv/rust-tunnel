import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useDnsConfig, useUpdateDnsConfig } from '@/api/hooks';

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
    return <div className="text-center py-8 text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle>DNS Configuration</CardTitle>
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
