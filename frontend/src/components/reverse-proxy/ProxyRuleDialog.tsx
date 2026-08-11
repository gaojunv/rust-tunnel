import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
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
import { AlertTriangle } from 'lucide-react';
import { useQuery } from '@tanstack/react-query';
import { clientsApi } from '@/api/client';
import { useCreateProxyRule, useUpdateProxyRule } from '@/api/hooks';
import { HttpRouteFields } from './HttpRouteFields';
import type { ProxyRule, RuleType, Route, ProxyTlsConfig, CreateProxyRuleRequest } from '@/types';

interface ProxyRuleDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  editingRule?: ProxyRule | null;
}

const emptyForm = {
  name: '',
  type: 'http' as RuleType,
  listen: '',
  domains: [] as string[],
  routes: [] as Route[],
  tls: undefined as ProxyTlsConfig | undefined,
  enabled: true,
};

export function ProxyRuleDialog({ open, onOpenChange, editingRule }: ProxyRuleDialogProps) {
  const { t } = useTranslation();
  const createMutation = useCreateProxyRule();
  const updateMutation = useUpdateProxyRule();
  const [form, setForm] = useState(emptyForm);
  const { data: clientsData = [] } = useQuery({
    queryKey: ['clients'],
    queryFn: clientsApi.list,
  });

  useEffect(() => {
    if (editingRule) {
      setForm({
        name: editingRule.name,
        type: editingRule.type,
        listen: editingRule.listen,
        domains: editingRule.domains ?? [],
        routes: editingRule.routes ?? [],
        tls: editingRule.tls,
        enabled: editingRule.enabled,
      });
    } else {
      setForm(emptyForm);
    }
  }, [editingRule, open]);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();

    const data: CreateProxyRuleRequest = {
      name: form.name,
      type: form.type,
      listen: form.listen,
      enabled: form.enabled,
      ...(form.type === 'http'
        ? { domains: form.domains, routes: form.routes, tls: form.tls }
        : { routes: form.routes.length > 0 ? form.routes : undefined }),
    };

    const handleError = (error: unknown) => {
      const err = error as {
        response?: {
          status?: number;
          data?: { error?: string; conflicts?: Array<{ reason: string }> };
        };
      };
      if (err?.response?.status === 409) {
        const body = err.response.data ?? {};
        const details = (body.conflicts ?? [])
          .map((c) => c.reason)
          .join('; ');
        alert(t('reverseProxy.dialog.conflictAlert', {
          error: body.error ?? t('reverseProxy.dialog.conflictDefault'),
          details: details || '',
        }));
      }
    };

    if (editingRule) {
      updateMutation.mutate(
        { id: editingRule.id, data },
        {
          onSuccess: () => onOpenChange(false),
          onError: handleError,
        }
      );
    } else {
      createMutation.mutate(data, {
        onSuccess: () => onOpenChange(false),
        onError: handleError,
      });
    }
  };

  const mutation = editingRule ? updateMutation : createMutation;
  const tcpBackend = form.routes?.[0]?.backends?.[0];

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] overflow-y-auto max-w-2xl">
        <DialogHeader>
          <DialogTitle>{editingRule ? t('reverseProxy.dialog.edit') : t('reverseProxy.dialog.new')}</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-5">
          {/* Name + Type */}
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('reverseProxy.dialog.name')}</label>
              <Input
                value={form.name}
                onChange={(e) => setForm({ ...form, name: e.target.value })}
                placeholder="my-proxy-rule"
                required
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('reverseProxy.dialog.type')}</label>
              <Select
                value={form.type}
                onValueChange={(v) => setForm({ ...form, type: v as RuleType })}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="http">{t('reverseProxy.dialog.types.http')}</SelectItem>
                  <SelectItem value="tcp">{t('reverseProxy.dialog.types.tcp')}</SelectItem>
                  <SelectItem value="udp">{t('reverseProxy.dialog.types.udp')}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>

          {/* Listen Address */}
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('reverseProxy.dialog.listenAddress')}</label>
            <Input
              value={form.listen}
              onChange={(e) => setForm({ ...form, listen: e.target.value })}
              placeholder="0.0.0.0:8080"
              className="font-mono"
              required
            />
          </div>

          {/* HTTP-specific fields */}
          {form.type === 'http' && (
            <HttpRouteFields
              domains={form.domains}
              onDomainsChange={(domains) => setForm({ ...form, domains })}
              routes={form.routes}
              onRoutesChange={(routes) => setForm({ ...form, routes })}
              tls={form.tls}
              onTlsChange={(tls) => setForm({ ...form, tls })}
            />
          )}

          {/* TCP/UDP backend */}
          {(form.type === 'tcp' || form.type === 'udp') && (
            <div className="space-y-3">
              <label className="text-sm font-medium">{t('reverseProxy.dialog.backend')}</label>
              {/* Kind radio */}
              <div className="flex items-center gap-4">
                <label className="flex items-center gap-1.5 text-sm">
                  <input
                    type="radio"
                    checked={tcpBackend?.kind !== 'client'}
                    onChange={() =>
                      setForm({
                        ...form,
                        routes: [
                          {
                            path: '/',
                            backends: [{
                              kind: 'direct',
                              addr: tcpBackend?.addr ?? '',
                              weight: 100,
                              client_name: null,
                            }],
                            load_balancing: 'round_robin',
                          },
                        ],
                      })
                    }
                    className="h-3.5 w-3.5"
                  />
                  {t('reverseProxy.backendFields.direct')}
                </label>
                <label className="flex items-center gap-1.5 text-sm">
                  <input
                    type="radio"
                    checked={tcpBackend?.kind === 'client'}
                    onChange={() =>
                      setForm({
                        ...form,
                        routes: [
                          {
                            path: '/',
                            backends: [{
                              kind: 'client',
                              addr: tcpBackend?.addr ?? '',
                              weight: 100,
                              client_name: '',
                            }],
                            load_balancing: 'round_robin',
                          },
                        ],
                      })
                    }
                    className="h-3.5 w-3.5"
                  />
                  {t('reverseProxy.backendFields.client')}
                </label>
              </div>
              {/* Client name field */}
              {tcpBackend?.kind === 'client' && (
                <div>
                  <label className="mb-1 block text-xs text-muted-foreground">{t('reverseProxy.backendFields.clientName')}</label>
                  <input
                    list="tcp-clients-datalist"
                    value={tcpBackend.client_name ?? ''}
                    onChange={(e) =>
                      setForm({
                        ...form,
                        routes: [
                          {
                            path: '/',
                            backends: [{ ...tcpBackend, client_name: e.target.value || null }],
                            load_balancing: 'round_robin',
                          },
                        ],
                      })
                    }
                    placeholder={t('reverseProxy.backendFields.clientNamePlaceholder')}
                    className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm transition-colors file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                  />
                  <datalist id="tcp-clients-datalist">
                    {clientsData.map((c) => (
                      <option key={c.name} value={c.name}>
                        {c.online ? t('common.status.online') : t('common.status.offline')} {c.hostname ?? ''}
                      </option>
                    ))}
                  </datalist>
                </div>
              )}
              {/* Backend address */}
              <Input
                value={tcpBackend?.addr ?? ''}
                onChange={(e) =>
                  setForm({
                    ...form,
                    routes: [
                      {
                        path: '/',
                        backends: [{ ...(tcpBackend ?? { kind: 'direct', weight: 100, client_name: null }), addr: e.target.value }],
                        load_balancing: 'round_robin',
                      },
                    ],
                  })
                }
                placeholder="127.0.0.1:3000"
                className="font-mono"
                required
              />
            </div>
          )}

          {/* Enabled */}
          <div className="flex items-center justify-between rounded-lg border bg-muted/30 p-4">
            <div>
              <div className="text-sm font-medium">{t('reverseProxy.dialog.enabled')}</div>
              <div className="text-xs text-muted-foreground">{t('reverseProxy.dialog.enabledDesc')}</div>
            </div>
            <Switch
              checked={form.enabled}
              onCheckedChange={(checked) => setForm({ ...form, enabled: checked })}
            />
          </div>

          {/* Error */}
          {mutation.isError && (
            <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <AlertTriangle className="h-4 w-4 shrink-0" />
              {t('reverseProxy.dialog.error', { action: editingRule ? t('reverseProxy.dialog.errorUpdate') : t('reverseProxy.dialog.errorCreate') })}
            </div>
          )}

          {/* Submit */}
          <Button type="submit" disabled={mutation.isPending} className="w-full">
            {mutation.isPending
              ? t('common.saving')
              : editingRule
                ? t('reverseProxy.dialog.submit.update')
                : t('reverseProxy.dialog.submit.create')}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
