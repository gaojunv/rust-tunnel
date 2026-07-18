import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useSettings, useAcmeStatus } from '@/api/hooks';
import { Info, Server, Lock, ArrowLeftRight } from 'lucide-react';

const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

export default function GeneralTab() {
  const { data: settings, isLoading } = useSettings();
  const { data: acmeStatus } = useAcmeStatus();

  const [apiTls, setApiTls] = useState(false);
  const [apiDomain, setApiDomain] = useState('');

  useEffect(() => {
    if (settings) {
      setApiTls(settings.api_tls ?? false);
      setApiDomain(settings.api_domain ?? '');
    }
  }, [settings]);

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      {/* Log Level (read-only display) */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Server className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">System</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            <label className="text-sm font-medium">Log Level</label>
            <Select value={settings?.log_level ?? 'info'} disabled>
              <SelectTrigger className="w-48">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {LOG_LEVELS.map((l) => (
                  <SelectItem key={l} value={l}>
                    {l}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground">
              Log level is managed via the server config file or environment variables.
            </p>
          </div>
        </CardContent>
      </Card>

      {/* API Server TLS */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Lock className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">API Server TLS</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <label className="text-sm font-medium">Enable TLS</label>
              <p className="text-xs text-muted-foreground">
                Serve the API over HTTPS using ACME certificates
              </p>
            </div>
            <Switch
              checked={apiTls}
              onCheckedChange={setApiTls}
              disabled
            />
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">API Domain</label>
            <Input
              value={apiDomain}
              onChange={(e) => setApiDomain(e.target.value)}
              placeholder="api.example.com"
              disabled
            />
            <p className="text-xs text-muted-foreground">
              Domain name for the API server TLS certificate. Requires ACME to be enabled.
            </p>
          </div>

          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p className="mb-1 font-medium text-foreground">Configuration via server config file</p>
              <p>
                API TLS settings (
                <code className="rounded border bg-background/60 px-1 py-0.5 text-xs">api_tls</code>,{' '}
                <code className="rounded border bg-background/60 px-1 py-0.5 text-xs">api_domain</code>
                ) require a server restart to take effect. Configure them in the TOML config file
                or via environment variables.
              </p>
              {!acmeStatus?.enabled && (
                <p className="mt-2 text-amber-500">
                  ⚠ ACME is not enabled. Enable ACME first to use API TLS.
                </p>
              )}
            </div>
          </div>
        </CardContent>
      </Card>

      {/* Reverse Proxy */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <ArrowLeftRight className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">Reverse Proxy</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p>
                Reverse proxy rules are managed on the{' '}
                <a
                  href="/proxy"
                  className="font-medium text-primary underline underline-offset-4 hover:text-primary/80"
                >
                  Reverse Proxy
                </a>{' '}
                page. Create and configure HTTP, TCP, and UDP proxy rules there.
              </p>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
