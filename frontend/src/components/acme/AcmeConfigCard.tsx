import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Badge } from '@/components/ui/badge';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { ChevronDown, Settings } from 'lucide-react';
import { useAcmeConfig, useUpdateAcmeConfig } from '@/api/hooks';
import { cn } from '@/lib/utils';
import type { UpdateAcmeConfigRequest } from '@/types';

export function AcmeConfigCard() {
  const { t } = useTranslation();
  const { data: config, isLoading } = useAcmeConfig();
  const updateMutation = useUpdateAcmeConfig();
  const [editOpen, setEditOpen] = useState(false);
  const [form, setForm] = useState<UpdateAcmeConfigRequest>({});

  useEffect(() => {
    if (config) {
      setForm({
        enabled: config.enabled,
        server_url: config.server_url,
        email: config.email,
        auto_renew: config.auto_renew,
        renewal_check_interval: config.renewal_check_interval,
        renewal_days_before_expiry: config.renewal_days_before_expiry,
        tos_agreed: config.tos_agreed,
      });
    }
  }, [config]);

  const handleSave = (e: React.FormEvent) => {
    e.preventDefault();
    updateMutation.mutate(form, {
      onSuccess: () => setEditOpen(false),
    });
  };

  if (isLoading) {
    return (
      <Card>
        <CardContent className="py-8 text-center text-muted-foreground">{t('common.loading')}</CardContent>
      </Card>
    );
  }

  if (!config) return null;

  return (
    <>
      <Collapsible defaultOpen>
        <Card>
          <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
            <CardTitle className="flex min-w-0 items-center gap-2">
              {t('acme.config.title')}
              <Badge
                variant="outline"
                className={cn(
                  'gap-1.5 font-medium',
                  config.enabled
                    ? 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25'
                    : 'text-muted-foreground'
                )}
              >
                <span
                  className={cn(
                    'h-1.5 w-1.5 rounded-full',
                    config.enabled
                      ? 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]'
                      : 'bg-muted-foreground/50'
                  )}
                />
                {config.enabled ? t('common.status.active') : t('common.status.inactive')}
              </Badge>
            </CardTitle>
            <div className="flex items-center gap-2">
              <CollapsibleTrigger asChild>
                <Button variant="ghost" size="icon">
                  <ChevronDown className="h-4 w-4" />
                </Button>
              </CollapsibleTrigger>
              <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
                <Settings className="mr-2 h-4 w-4" />
                {t('acme.config.edit')}
              </Button>
            </div>
          </CardHeader>
          <CollapsibleContent>
            <CardContent>
              <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-4 text-sm">
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.serverUrl')}</div>
                  <div className="font-mono truncate">{config.server_url}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.email')}</div>
                  <div>{config.email || '—'}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.certDir')}</div>
                  <div className="font-mono">{config.cert_dir}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.autoRenew')}</div>
                  <div>{config.auto_renew ? t('common.yes') : t('common.no')}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.checkInterval')}</div>
                  <div>{config.renewal_check_interval}h</div>
                </div>
                <div>
                  <div className="text-muted-foreground">{t('acme.config.fields.daysBeforeExpiry')}</div>
                  <div>{config.renewal_days_before_expiry} {t('acme.config.fields.daysUnit')}</div>
                </div>
              </div>
            </CardContent>
          </CollapsibleContent>
        </Card>
      </Collapsible>

      {/* Edit Dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-h-[85vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('acme.config.dialog.title')}</DialogTitle>
          </DialogHeader>
          <form onSubmit={handleSave} className="space-y-4">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">{t('acme.config.dialog.enable')}</div>
                <div className="text-xs text-muted-foreground">{t('acme.config.dialog.enableDesc')}</div>
              </div>
              <Switch
                checked={form.enabled ?? false}
                onCheckedChange={(checked) => setForm({ ...form, enabled: checked })}
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.config.dialog.serverUrl')}</label>
              <Input
                value={form.server_url ?? ''}
                onChange={(e) => setForm({ ...form, server_url: e.target.value })}
                placeholder="https://acme-v02.api.letsencrypt.org/directory"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.config.dialog.email')}</label>
              <Input
                type="email"
                value={form.email ?? ''}
                onChange={(e) => setForm({ ...form, email: e.target.value })}
                placeholder="admin@example.com"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.config.dialog.certDir')}</label>
              <Input value={config.cert_dir} disabled />
              <p className="text-xs text-muted-foreground">{t('acme.config.dialog.certDirHint')}</p>
            </div>

            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">{t('acme.config.dialog.autoRenew')}</div>
                <div className="text-xs text-muted-foreground">{t('acme.config.dialog.autoRenewDesc')}</div>
              </div>
              <Switch
                checked={form.auto_renew ?? false}
                onCheckedChange={(checked) => setForm({ ...form, auto_renew: checked })}
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.config.dialog.checkInterval')}</label>
              <Input
                type="number"
                value={form.renewal_check_interval ?? 24}
                onChange={(e) =>
                  setForm({ ...form, renewal_check_interval: parseInt(e.target.value, 10) || 24 })
                }
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">{t('acme.config.dialog.daysBeforeExpiry')}</label>
              <Input
                type="number"
                value={form.renewal_days_before_expiry ?? 30}
                onChange={(e) =>
                  setForm({
                    ...form,
                    renewal_days_before_expiry: parseInt(e.target.value, 10) || 30,
                  })
                }
              />
            </div>

            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">{t('acme.config.dialog.tosAgree')}</div>
                <div className="text-xs text-muted-foreground">
                  {t('acme.config.dialog.tosDesc')}
                </div>
              </div>
              <Switch
                checked={form.tos_agreed ?? false}
                onCheckedChange={(checked) => setForm({ ...form, tos_agreed: checked })}
              />
            </div>

            {form.enabled && !form.tos_agreed && (
              <p className="text-sm text-destructive">
                {t('acme.config.dialog.tosError')}
              </p>
            )}

            {updateMutation.isError && (
              <p className="text-sm text-destructive">
                {t('acme.config.dialog.saveError')}
              </p>
            )}

            <Button
              type="submit"
              disabled={updateMutation.isPending || (form.enabled && !form.tos_agreed)}
              className="w-full"
            >
              {updateMutation.isPending ? t('common.saving') : t('acme.config.dialog.save')}
            </Button>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
