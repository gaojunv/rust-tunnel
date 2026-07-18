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
import { useShadowsocksConfig, useUpdateShadowsocksConfig } from '@/api/hooks';
import { Shield } from 'lucide-react';

export default function ShadowsocksConfigCard() {
  const { data: ssConfig, isLoading } = useShadowsocksConfig();
  const updateSS = useUpdateShadowsocksConfig();

  const [ssEnabled, setSsEnabled] = useState(false);
  const [ssPort, setSsPort] = useState('8388');
  const [ssCipher, setSsCipher] = useState('aes-256-gcm');
  const [ssPassword, setSsPassword] = useState('');

  useEffect(() => {
    if (ssConfig) {
      setSsEnabled(ssConfig.enabled ?? false);
      setSsPort(ssConfig.port?.toString() ?? '8388');
      setSsCipher(ssConfig.cipher ?? 'aes-256-gcm');
      // Password not returned from API for security
    }
  }, [ssConfig]);

  const handleSaveSS = () => {
    if (!ssPassword && !ssConfig?.enabled) {
      return; // Password required when enabling
    }
    updateSS.mutate({
      enabled: ssEnabled,
      port: parseInt(ssPort, 10),
      cipher: ssCipher,
      ...(ssPassword && { password: ssPassword }),
    });
  };

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">Loading...</div>;
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Shield className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">Shadowsocks Proxy</CardTitle>
          </div>
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
          <div className="space-y-2">
            <label className="text-sm font-medium">Password</label>
            <Input
              type="password"
              value={ssPassword}
              onChange={(e) => setSsPassword(e.target.value)}
              placeholder={ssConfig?.enabled ? '••••••••' : 'Enter password'}
            />
            <p className="text-xs text-muted-foreground">
              {ssConfig?.enabled ? 'Leave blank to keep current password' : 'Required to enable'}
            </p>
          </div>
        </div>
        <Button onClick={handleSaveSS} disabled={updateSS.isPending}>
          {updateSS.isPending ? 'Saving...' : 'Save Shadowsocks Config'}
        </Button>
      </CardContent>
    </Card>
  );
}
