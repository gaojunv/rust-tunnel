import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Settings, Globe } from 'lucide-react';
import { useDnsProviders, useUpdateDnsProvider } from '@/api/hooks';
import type { DnsProviderType, DnsProviderConfig } from '@/types';

const PROVIDER_LABELS: Record<DnsProviderType, string> = {
  cloudflare: 'Cloudflare',
  aliyun: 'Aliyun DNS',
  tencent: 'Tencent DNS',
  custom: 'Custom',
};

export function DnsProviderConfigCard() {
  const { data, isLoading } = useDnsProviders();
  const updateMutation = useUpdateDnsProvider();
  const [editOpen, setEditOpen] = useState(false);
  const [form, setForm] = useState<DnsProviderConfig>({
    provider: 'cloudflare',
    api_key: '',
    api_secret: '',
    zone_id: '',
  });

  useEffect(() => {
    if (data?.config) {
      setForm(data.config);
    }
  }, [data]);

  const handleSave = (e: React.FormEvent) => {
    e.preventDefault();
    updateMutation.mutate(form, {
      onSuccess: () => setEditOpen(false),
    });
  };

  if (isLoading) {
    return (
      <Card>
        <CardContent className="py-8 text-center text-muted-foreground">
          Loading...
        </CardContent>
      </Card>
    );
  }

  return (
    <>
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="flex items-center gap-2">
            <Globe className="h-5 w-5" />
            DNS Provider
            <Badge variant={data?.config ? 'default' : 'secondary'}>
              {data?.config ? PROVIDER_LABELS[data.config.provider] : 'Not Configured'}
            </Badge>
          </CardTitle>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setEditOpen(true)}
          >
            <Settings className="mr-2 h-4 w-4" />
            Configure
          </Button>
        </CardHeader>
        <CardContent>
          {data?.config ? (
            <div className="grid gap-4 md:grid-cols-3 text-sm">
              <div>
                <div className="text-muted-foreground">Provider</div>
                <div>{PROVIDER_LABELS[data.config.provider]}</div>
              </div>
              <div>
                <div className="text-muted-foreground">API Key</div>
                <div className="font-mono">
                  {data.config.api_key
                    ? `${data.config.api_key.slice(0, 8)}...`
                    : '—'}
                </div>
              </div>
              {data.config.zone_id && (
                <div>
                  <div className="text-muted-foreground">Zone ID</div>
                  <div className="font-mono truncate">{data.config.zone_id}</div>
                </div>
              )}
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">
              No DNS provider configured. Configure a DNS provider to enable
              DNS-01 challenge validation for ACME certificates.
            </p>
          )}
        </CardContent>
      </Card>

      {/* Edit Dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-h-[85vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>Configure DNS Provider</DialogTitle>
          </DialogHeader>
          <form onSubmit={handleSave} className="space-y-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">Provider</label>
              <Select
                value={form.provider}
                onValueChange={(value) =>
                  setForm({ ...form, provider: value as DnsProviderType })
                }
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="cloudflare">Cloudflare</SelectItem>
                  <SelectItem value="aliyun">Aliyun DNS</SelectItem>
                  <SelectItem value="tencent">Tencent DNS</SelectItem>
                  <SelectItem value="custom">Custom</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">API Key</label>
              <Input
                value={form.api_key}
                onChange={(e) =>
                  setForm({ ...form, api_key: e.target.value })
                }
                placeholder="Enter API key"
                required
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">
                API Secret
                <span className="text-xs text-muted-foreground ml-1">
                  (optional)
                </span>
              </label>
              <Input
                type="password"
                value={form.api_secret ?? ''}
                onChange={(e) =>
                  setForm({ ...form, api_secret: e.target.value })
                }
                placeholder="Enter API secret"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">
                Zone ID
                <span className="text-xs text-muted-foreground ml-1">
                  (optional)
                </span>
              </label>
              <Input
                value={form.zone_id ?? ''}
                onChange={(e) =>
                  setForm({ ...form, zone_id: e.target.value })
                }
                placeholder="Enter zone ID"
              />
            </div>

            <p className="text-xs text-muted-foreground">
              DNS provider credentials are used for DNS-01 challenge validation.
              They are stored securely on the server.
            </p>

            {updateMutation.isError && (
              <p className="text-sm text-destructive">
                Failed to save DNS provider configuration. Please try again.
              </p>
            )}

            <Button
              type="submit"
              disabled={updateMutation.isPending}
              className="w-full"
            >
              {updateMutation.isPending ? 'Saving...' : 'Save Configuration'}
            </Button>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
