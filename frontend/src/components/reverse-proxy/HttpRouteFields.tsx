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
import { Plus, Trash2 } from 'lucide-react';
import { BackendFields } from './BackendFields';
import type { Route, LoadBalancing, ProxyTlsConfig } from '@/types';

interface HttpRouteFieldsProps {
  domains: string[];
  onDomainsChange: (domains: string[]) => void;
  routes: Route[];
  onRoutesChange: (routes: Route[]) => void;
  tls?: ProxyTlsConfig;
  onTlsChange: (tls: ProxyTlsConfig | undefined) => void;
}

export function HttpRouteFields({
  domains,
  onDomainsChange,
  routes,
  onRoutesChange,
  tls,
  onTlsChange,
}: HttpRouteFieldsProps) {
  const addRoute = () => {
    onRoutesChange([
      ...routes,
      { path: '/', backends: [{ addr: '', weight: 100 }], load_balancing: 'round_robin' },
    ]);
  };

  const removeRoute = (index: number) => {
    onRoutesChange(routes.filter((_, i) => i !== index));
  };

  const updateRoute = (index: number, updates: Partial<Route>) => {
    onRoutesChange(routes.map((r, i) => (i === index ? { ...r, ...updates } : r)));
  };

  return (
    <div className="space-y-4">
      {/* Domains */}
      <div className="space-y-2">
        <label className="text-sm font-medium">Domains</label>
        <Input
          value={domains.join(', ')}
          onChange={(e) =>
            onDomainsChange(
              e.target.value
                .split(',')
                .map((d) => d.trim())
                .filter(Boolean)
            )
          }
          placeholder="example.com, api.example.com"
        />
        <p className="text-xs text-muted-foreground">
          Comma-separated domain names
        </p>
      </div>

      {/* Routes */}
      <div className="space-y-4">
        <div className="flex items-center justify-between">
          <label className="text-sm font-medium">Routes</label>
          <Button type="button" variant="outline" size="sm" onClick={addRoute}>
            <Plus className="mr-2 h-4 w-4" />
            Add Route
          </Button>
        </div>
        {routes.map((route, index) => (
          <div key={index} className="rounded-md border p-4 space-y-3">
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium">Route {index + 1}</span>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => removeRoute(index)}
              >
                <Trash2 className="h-4 w-4 text-destructive" />
              </Button>
            </div>
            <Input
              value={route.path}
              onChange={(e) => updateRoute(index, { path: e.target.value })}
              placeholder="/api"
            />
            <BackendFields
              backends={route.backends}
              onChange={(backends) => updateRoute(index, { backends })}
            />
            <div className="space-y-2">
              <label className="text-sm font-medium">Load Balancing</label>
              <Select
                value={route.load_balancing}
                onValueChange={(v) =>
                  updateRoute(index, { load_balancing: v as LoadBalancing })
                }
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="round_robin">Round Robin</SelectItem>
                  <SelectItem value="weighted_round_robin">Weighted Round Robin</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
        ))}
      </div>

      {/* TLS */}
      <div className="space-y-3 rounded-md border p-4">
        <div className="flex items-center justify-between">
          <div>
            <div className="text-sm font-medium">TLS</div>
            <div className="text-xs text-muted-foreground">Enable HTTPS</div>
          </div>
          <Switch
            checked={tls?.enabled ?? false}
            onCheckedChange={(checked) =>
              onTlsChange(
                checked
                  ? { enabled: true, acme: false, domain: '' }
                  : undefined
              )
            }
          />
        </div>
        {tls?.enabled && (
          <>
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">ACME Certificate</div>
                <div className="text-xs text-muted-foreground">
                  Auto-manage certificate via ACME
                </div>
              </div>
              <Switch
                checked={tls.acme}
                onCheckedChange={(checked) =>
                  onTlsChange({ ...tls, acme: checked })
                }
              />
            </div>
            {tls.acme && (
              <Input
                value={tls.domain ?? ''}
                onChange={(e) =>
                  onTlsChange({ ...tls, domain: e.target.value })
                }
                placeholder="example.com"
              />
            )}
          </>
        )}
      </div>
    </div>
  );
}
