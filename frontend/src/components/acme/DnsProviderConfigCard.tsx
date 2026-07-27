import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
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
import { cn } from '@/lib/utils';
import type { DnsProviderType, DnsProviderConfig } from '@/types';

const PROVIDER_LABEL_KEYS = {
  cloudflare: 'acme.dnsProvider.providerLabels.cloudflare',
  aliyun: 'acme.dnsProvider.providerLabels.aliyun',
  tencent: 'acme.dnsProvider.providerLabels.tencent',
  custom: 'acme.dnsProvider.providerLabels.custom',
} as const;

export function DnsProviderConfigCard() {
  const { t } = useTranslation();
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
          {t('common.loading')}
        </CardContent>
      </Card>
    );
  }

  const providerLabel = data?.config
    ? t(PROVIDER_LABEL_KEYS[data.config.provider] ?? data.config.provider)
    : t('acme.dnsProvider.notConfigured');

  return (
    <>
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="flex items-center gap-2">
            <Globe className="h-5 w-5 text-primary" />
            {t('acme.dnsProvider.title')}
            <Badge
              variant="outline"
              className={cn(
                'gap-1.5 font-medium',
                data?.config
                  ? 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25'
                  : 'text-muted-foreground'
              )}
            >
              <span
                className={cn(
                  'h-1.5 w-1.5 rounded-full',
                  data?.config
                    ? 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]'
                    : 'bg-muted-foreground/50'
                )}
              />
              {providerLabel}
            </Badge>
          </CardTitle>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setEditOpen(true)}
          >
            <Settings className="mr-2 h-4 w-4" />
            {t('acme.dnsProvider.configure')}
          </Button>
        </CardHeader>
        <CardContent>
          {data?.config ? (
            <div className="grid gap-4 md:grid-cols-3 text-sm">
              <div>
                <div className="text-muted-foreground">{t('acme.dnsProvider.provider')}</div>
                <div>{providerLabel}</div>
              </div>
              <div>
                <div className="text-muted-foreground">{t('acme.dnsProvider.apiKey')}</div>
                <div className="font-mono">
                  {data.config.api_key
                    ? `${data.config.api_key.slice(0, 8)}...`
                    : '—'}
                </div>
              </div>
              {data.config.zone_id && (
                <div>
                  <div className="text-muted-foreground">{t('acme.dnsProvider.zoneId')}</div>
                  <div className="font-mono truncate">{data.config.zone_id}</div>
                </div>
              )}
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">
              {t('acme.dnsProvider.empty')}
            </p>
          )}
        </CardContent>
      </Card>

      {/* Edit Dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-h-[85vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('acme.dnsProvider.dialog.title')}</DialogTitle>
          </DialogHeader>
          <form onSubmit={handleSave} className="space-y-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.dnsProvider.provider')}</label>
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
                  <SelectItem value="cloudflare">{t('acme.dnsProvider.providerLabels.cloudflare')}</SelectItem>
                  <SelectItem value="aliyun">{t('acme.dnsProvider.providerLabels.aliyun')}</SelectItem>
                  <SelectItem value="tencent">{t('acme.dnsProvider.providerLabels.tencent')}</SelectItem>
                  <SelectItem value="custom">{t('acme.dnsProvider.providerLabels.custom')}</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.dnsProvider.apiKey')}</label>
              <Input
                value={form.api_key}
                onChange={(e) =>
                  setForm({ ...form, api_key: e.target.value })
                }
                placeholder={t('acme.dnsProvider.apiKey')}
                required
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">
                {t('acme.dnsProvider.dialog.apiSecret')}
                <span className="text-xs text-muted-foreground ml-1">
                  {t('common.optional')}
                </span>
              </label>
              <Input
                type="password"
                value={form.api_secret ?? ''}
                onChange={(e) =>
                  setForm({ ...form, api_secret: e.target.value })
                }
                placeholder={t('acme.dnsProvider.dialog.apiSecret')}
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">
                {t('acme.dnsProvider.dialog.zoneId')}
                <span className="text-xs text-muted-foreground ml-1">
                  {t('common.optional')}
                </span>
              </label>
              <Input
                value={form.zone_id ?? ''}
                onChange={(e) =>
                  setForm({ ...form, zone_id: e.target.value })
                }
                placeholder={t('acme.dnsProvider.dialog.zoneId')}
              />
            </div>

            <p className="text-xs text-muted-foreground">
              {t('acme.dnsProvider.dialog.hint')}
            </p>

            {updateMutation.isError && (
              <p className="text-sm text-destructive">
                {t('acme.dnsProvider.dialog.error')}
              </p>
            )}

            <Button
              type="submit"
              disabled={updateMutation.isPending}
              className="w-full"
            >
              {updateMutation.isPending ? t('common.saving') : t('acme.dnsProvider.dialog.save')}
            </Button>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
