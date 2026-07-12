import { useState, useEffect } from 'react';
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
import type { UpdateAcmeConfigRequest } from '@/types';

export function AcmeConfigCard() {
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
        <CardContent className="py-8 text-center text-muted-foreground">Loading...</CardContent>
      </Card>
    );
  }

  if (!config) return null;

  return (
    <>
      <Collapsible defaultOpen>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between">
            <CardTitle className="flex items-center gap-2">
              ACME Configuration
              <Badge variant={config.enabled ? 'default' : 'secondary'}>
                {config.enabled ? 'Enabled' : 'Disabled'}
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
                Edit
              </Button>
            </div>
          </CardHeader>
          <CollapsibleContent>
            <CardContent>
              <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4 text-sm">
                <div>
                  <div className="text-muted-foreground">Server URL</div>
                  <div className="font-mono truncate">{config.server_url}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">Email</div>
                  <div>{config.email || '—'}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">Cert Directory</div>
                  <div className="font-mono">{config.cert_dir}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">Auto Renew</div>
                  <div>{config.auto_renew ? 'Yes' : 'No'}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">Check Interval</div>
                  <div>{config.renewal_check_interval}h</div>
                </div>
                <div>
                  <div className="text-muted-foreground">Days Before Expiry</div>
                  <div>{config.renewal_days_before_expiry} days</div>
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
            <DialogTitle>Edit ACME Configuration</DialogTitle>
          </DialogHeader>
          <form onSubmit={handleSave} className="space-y-4">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">Enable ACME</div>
                <div className="text-xs text-muted-foreground">Enable automatic certificate management</div>
              </div>
              <Switch
                checked={form.enabled ?? false}
                onCheckedChange={(checked) => setForm({ ...form, enabled: checked })}
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Server URL</label>
              <Input
                value={form.server_url ?? ''}
                onChange={(e) => setForm({ ...form, server_url: e.target.value })}
                placeholder="https://acme-v02.api.letsencrypt.org/directory"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Email</label>
              <Input
                type="email"
                value={form.email ?? ''}
                onChange={(e) => setForm({ ...form, email: e.target.value })}
                placeholder="admin@example.com"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Certificate Directory</label>
              <Input value={config.cert_dir} disabled />
              <p className="text-xs text-muted-foreground">Cannot be changed via API</p>
            </div>

            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">Auto Renew</div>
                <div className="text-xs text-muted-foreground">Automatically renew certificates</div>
              </div>
              <Switch
                checked={form.auto_renew ?? false}
                onCheckedChange={(checked) => setForm({ ...form, auto_renew: checked })}
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Renewal Check Interval (hours)</label>
              <Input
                type="number"
                value={form.renewal_check_interval ?? 24}
                onChange={(e) =>
                  setForm({ ...form, renewal_check_interval: parseInt(e.target.value, 10) || 24 })
                }
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Days Before Expiry</label>
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
                <div className="text-sm font-medium">Agree to ToS</div>
                <div className="text-xs text-muted-foreground">
                  Required to use ACME certificates
                </div>
              </div>
              <Switch
                checked={form.tos_agreed ?? false}
                onCheckedChange={(checked) => setForm({ ...form, tos_agreed: checked })}
              />
            </div>

            {form.enabled && !form.tos_agreed && (
              <p className="text-sm text-destructive">
                You must agree to the Terms of Service to enable ACME
              </p>
            )}

            {updateMutation.isError && (
              <p className="text-sm text-destructive">
                Failed to save configuration. Please try again.
              </p>
            )}

            <Button
              type="submit"
              disabled={updateMutation.isPending || (form.enabled && !form.tos_agreed)}
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
