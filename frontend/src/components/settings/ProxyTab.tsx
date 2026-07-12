import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  useShadowsocksConfig,
  useUpdateShadowsocksConfig,
  useTrojanConfig,
  useUpdateTrojanConfig,
} from '@/api/hooks';

export default function ProxyTab() {
  const { data: ssConfig, isLoading: ssLoading } = useShadowsocksConfig();
  const { data: tjConfig, isLoading: tjLoading } = useTrojanConfig();
  const updateSS = useUpdateShadowsocksConfig();
  const updateTJ = useUpdateTrojanConfig();

  const [ssEnabled, setSsEnabled] = useState(false);
  const [ssPort, setSsPort] = useState('8388');
  const [ssCipher, setSsCipher] = useState('aes-256-gcm');

  const [tjEnabled, setTjEnabled] = useState(false);
  const [tjPort, setTjPort] = useState('443');
  const [tjFallback, setTjFallback] = useState('127.0.0.1:80');

  useEffect(() => {
    if (ssConfig) {
      setSsEnabled(ssConfig.enabled ?? false);
      setSsPort(ssConfig.port?.toString() ?? '8388');
      setSsCipher(ssConfig.cipher ?? 'aes-256-gcm');
    }
  }, [ssConfig]);

  useEffect(() => {
    if (tjConfig) {
      setTjEnabled(tjConfig.enabled ?? false);
      setTjPort(tjConfig.port?.toString() ?? '443');
      setTjFallback(tjConfig.fallback ?? '127.0.0.1:80');
    }
  }, [tjConfig]);

  const handleSaveSS = () => {
    updateSS.mutate({
      enabled: ssEnabled,
      port: parseInt(ssPort, 10),
      cipher: ssCipher,
    });
  };

  const handleSaveTJ = () => {
    updateTJ.mutate({
      enabled: tjEnabled,
      port: parseInt(tjPort, 10),
      fallback: tjFallback || undefined,
    });
  };

  if (ssLoading || tjLoading) {
    return <div className="text-center py-8 text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      {/* Shadowsocks */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <CardTitle>Shadowsocks Proxy</CardTitle>
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">Enable</span>
              <Switch checked={ssEnabled} onCheckedChange={setSsEnabled} />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div className="space-y-2">
              <label className="text-sm font-medium">Port</label>
              <Input
                type="number"
                value={ssPort}
                onChange={(e) => setSsPort(e.target.value)}
                placeholder="8388"
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">Cipher</label>
              <Select value={ssCipher} onValueChange={setSsCipher}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="aes-256-gcm">AES-256-GCM</SelectItem>
                  <SelectItem value="chacha20-ietf-poly1305">
                    ChaCha20-IETF-Poly1305
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
          <Button onClick={handleSaveSS} disabled={updateSS.isPending}>
            {updateSS.isPending ? 'Saving...' : 'Save Shadowsocks Config'}
          </Button>
        </CardContent>
      </Card>

      {/* Trojan */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <CardTitle>Trojan Proxy</CardTitle>
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">Enable</span>
              <Switch checked={tjEnabled} onCheckedChange={setTjEnabled} />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div className="space-y-2">
              <label className="text-sm font-medium">Port</label>
              <Input
                type="number"
                value={tjPort}
                onChange={(e) => setTjPort(e.target.value)}
                placeholder="443"
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">Fallback</label>
              <Input
                value={tjFallback}
                onChange={(e) => setTjFallback(e.target.value)}
                placeholder="127.0.0.1:80"
              />
              <p className="text-xs text-muted-foreground">
                Address to redirect traffic to when authentication fails
              </p>
            </div>
          </div>
          <Button onClick={handleSaveTJ} disabled={updateTJ.isPending}>
            {updateTJ.isPending ? 'Saving...' : 'Save Trojan Config'}
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
