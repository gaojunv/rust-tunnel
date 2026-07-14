import { useState, useEffect } from 'react';
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
  const createMutation = useCreateProxyRule();
  const updateMutation = useUpdateProxyRule();
  const [form, setForm] = useState(emptyForm);

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

    if (editingRule) {
      updateMutation.mutate(
        { id: editingRule.id, data },
        { onSuccess: () => onOpenChange(false) }
      );
    } else {
      createMutation.mutate(data, {
        onSuccess: () => onOpenChange(false),
      });
    }
  };

  const mutation = editingRule ? updateMutation : createMutation;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] overflow-y-auto max-w-2xl">
        <DialogHeader>
          <DialogTitle>{editingRule ? 'Edit Rule' : 'New Rule'}</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4">
          {/* Name */}
          <div className="space-y-2">
            <label className="text-sm font-medium">Name</label>
            <Input
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
              placeholder="my-proxy-rule"
              required
            />
          </div>

          {/* Rule Type */}
          <div className="space-y-2">
            <label className="text-sm font-medium">Type</label>
            <Select
              value={form.type}
              onValueChange={(v) => setForm({ ...form, type: v as RuleType })}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="http">HTTP</SelectItem>
                <SelectItem value="tcp">TCP</SelectItem>
                <SelectItem value="udp">UDP</SelectItem>
              </SelectContent>
            </Select>
          </div>

          {/* Listen Address */}
          <div className="space-y-2">
            <label className="text-sm font-medium">Listen Address</label>
            <Input
              value={form.listen}
              onChange={(e) => setForm({ ...form, listen: e.target.value })}
              placeholder="0.0.0.0:8080"
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
            <div className="space-y-2">
              <label className="text-sm font-medium">Backend Address</label>
              <Input
                value={form.routes?.[0]?.backends?.[0]?.addr ?? ''}
                onChange={(e) =>
                  setForm({
                    ...form,
                    routes: [
                      {
                        path: '/',
                        backends: [{ addr: e.target.value, weight: 100 }],
                        load_balancing: 'round_robin',
                      },
                    ],
                  })
                }
                placeholder="127.0.0.1:3000"
                required
              />
            </div>
          )}

          {/* Enabled */}
          <div className="flex items-center justify-between">
            <div>
              <div className="text-sm font-medium">Enabled</div>
              <div className="text-xs text-muted-foreground">Start this proxy rule immediately</div>
            </div>
            <Switch
              checked={form.enabled}
              onCheckedChange={(checked) => setForm({ ...form, enabled: checked })}
            />
          </div>

          {/* Error */}
          {mutation.isError && (
            <p className="text-sm text-destructive">
              Failed to {editingRule ? 'update' : 'create'} rule. Please try again.
            </p>
          )}

          {/* Submit */}
          <Button type="submit" disabled={mutation.isPending} className="w-full">
            {mutation.isPending
              ? 'Saving...'
              : editingRule
                ? 'Update Rule'
                : 'Create Rule'}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
