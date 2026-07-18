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
  useAcmeStatus,
} from '@/api/hooks';
import { Info, Shield, ShieldCheck } from 'lucide-react';

export default function ProxyTab() {
  const { data: ssConfig, isLoading: ssLoading } = useShadowsocksConfig();
  const { data: tjConfig, isLoading: tjLoading } = useTrojanConfig();
  const { data: acmeStatus } = useAcmeStatus();
  const updateSS = useUpdateShadowsocksConfig();
  const updateTJ = useUpdateTrojanConfig();

  const [ssEnabled, setSsEnabled] = useState(false);
  const [ssPort, setSsPort] = useState('8388');
  const [ssCipher, setSsCipher] = useState('aes-256-gcm');
  const [ssPassword, setSsPassword] = useState('');

  const [tjEnabled, setTjEnabled] = useState(false);
  const [tjPort, setTjPort] = useState('443');
  const [tjPassword, setTjPassword] = useState('');
  const [tjFallback, setTjFallback] = useState('127.0.0.1:80');

  useEffect(() => {
    if (ssConfig) {
      setSsEnabled(ssConfig.enabled ?? false);
      setSsPort(ssConfig.port?.toString() ?? '8388');
      setSsCipher(ssConfig.cipher ?? 'aes-256-gcm');
      // Password not returned from API for security
    }
  }, [ssConfig]);

  useEffect(() => {
    if (tjConfig) {
      setTjEnabled(tjConfig.enabled ?? false);
      setTjPort(tjConfig.port?.toString() ?? '443');
      setTjFallback(tjConfig.fallback ?? '127.0.0.1:80');
      // Password not returned from API for security
    }
  }, [tjConfig]);

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

  const handleSaveTJ = () => {
    if (!tjPassword && !tjConfig?.enabled) {
      return; // Password required when enabling
    }
    updateTJ.mutate({
      enabled: tjEnabled,
      port: parseInt(tjPort, 10),
      ...(tjPassword && { password: tjPassword }),
      fallback: tjFallback || undefined,
    });
  };

  if (ssLoading || tjLoading) {
    return <div className="py-8 text-center text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      {/* Shadowsocks */}
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

      {/* Trojan */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-3">
              <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
                <ShieldCheck className="h-4 w-4" />
              </div>
              <CardTitle className="text-lg">Trojan Proxy</CardTitle>
            </div>
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
              <label className="text-sm font-medium">Password</label>
              <Input
                type="password"
                value={tjPassword}
                onChange={(e) => setTjPassword(e.target.value)}
                placeholder={tjConfig?.enabled ? '••••••••' : 'Enter password'}
              />
              <p className="text-xs text-muted-foreground">
                {tjConfig?.enabled ? 'Leave blank to keep current password' : 'Required to enable'}
              </p>
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

          {/* Trojan TLS info */}
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p className="mb-1 font-medium text-foreground">TLS Certificate</p>
              <p>
                Trojan requires TLS. The certificate is managed via{' '}
                <a
                  href="/acme"
                  className="font-medium text-primary underline underline-offset-4 hover:text-primary/80"
                >
                  ACME
                </a>
                .{' '}
                {acmeStatus?.enabled ? (
                  <span className="text-emerald-500">
                    ACME is enabled. Trojan will use the certificate for the configured domain.
                  </span>
                ) : (
                  <span className="text-amber-500">
                    ⚠ ACME is not enabled. Enable ACME first to use Trojan.
                  </span>
                )}
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
