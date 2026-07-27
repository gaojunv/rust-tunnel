import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { AlertTriangle, Plus, Trash2 } from 'lucide-react';
import { clientsApi } from '@/api/client';
import type { Backend, BackendScheme, BackendProtocol } from '@/types';

interface BackendFieldsProps {
  backends: Backend[];
  onChange: (backends: Backend[]) => void;
}

export function BackendFields({ backends, onChange }: BackendFieldsProps) {
  const { t } = useTranslation();
  const { data: clientsData = [] } = useQuery({
    queryKey: ['clients'],
    queryFn: clientsApi.list,
  });

  const addBackend = () => {
    onChange([...backends, { kind: 'direct', addr: '', weight: 100, client_name: null }]);
  };

  const removeBackend = (index: number) => {
    onChange(backends.filter((_, i) => i !== index));
  };

  const updateBackend = (
    index: number,
    field: keyof Backend,
    value: string | number | null,
  ) => {
    const updated = backends.map((b, i) =>
      i === index ? { ...b, [field]: value } : b
    );
    onChange(updated);
  };

  return (
    <div className="space-y-2">
      <label className="text-sm font-medium">{t('reverseProxy.backendFields.backendServers')}</label>
      {backends.map((backend, index) => {
        const scheme = backend.scheme ?? 'http';
        const protocol = backend.protocol ?? 'http1';
        const showH2cHint = protocol === 'http2' && scheme === 'http';
        return (
          <div key={index} className="space-y-3 rounded-lg border bg-muted/30 p-3">
            {/* Kind radio */}
            <div className="flex items-center gap-4">
              <label className="flex items-center gap-1.5 text-sm">
                <input
                  type="radio"
                  checked={backend.kind === 'direct'}
                  onChange={() =>
                    updateBackend(index, 'kind', 'direct')
                  }
                  className="h-3.5 w-3.5"
                />
                {t('reverseProxy.backendFields.direct')}
              </label>
              <label className="flex items-center gap-1.5 text-sm">
                <input
                  type="radio"
                  checked={backend.kind === 'client'}
                  onChange={() => updateBackend(index, 'kind', 'client')}
                  className="h-3.5 w-3.5"
                />
                {t('reverseProxy.backendFields.client')}
              </label>
            </div>
            {/* Client name field (only for client kind) */}
            {backend.kind === 'client' && (
              <div>
                <label className="mb-1 block text-xs text-muted-foreground">{t('reverseProxy.backendFields.clientName')}</label>
                <input
                  list="clients-datalist"
                  value={backend.client_name ?? ''}
                  onChange={(e) =>
                    updateBackend(index, 'client_name', e.target.value || null)
                  }
                  placeholder={t('reverseProxy.backendFields.clientNamePlaceholder')}
                  className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm transition-colors file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                />
                <datalist id="clients-datalist">
                  {clientsData.map((c) => (
                    <option key={c.name} value={c.name}>
                      {c.online ? t('common.status.online') : t('common.status.offline')} {c.hostname ?? ''}
                    </option>
                  ))}
                </datalist>
              </div>
            )}
            {/* Addr + Weight row */}
            <div className="flex items-center gap-2">
              <Input
                value={backend.addr}
                onChange={(e) => updateBackend(index, 'addr', e.target.value)}
                placeholder="127.0.0.1:8080"
                className="flex-1 font-mono"
              />
              <Input
                type="number"
                value={backend.weight}
                onChange={(e) =>
                  updateBackend(index, 'weight', parseInt(e.target.value, 10) || 100)
                }
                placeholder={t('reverseProxy.backendFields.weightPlaceholder')}
                className="w-24"
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => removeBackend(index)}
              >
                <Trash2 className="h-4 w-4 text-destructive" />
              </Button>
            </div>
            <div className="flex items-center gap-2">
              <div className="flex-1 space-y-1">
                <label className="text-xs text-muted-foreground">{t('reverseProxy.backendFields.scheme')}</label>
                <Select
                  value={scheme}
                  onValueChange={(v) =>
                    updateBackend(index, 'scheme', v as BackendScheme)
                  }
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="http">http</SelectItem>
                    <SelectItem value="https">https</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="flex-1 space-y-1">
                <label className="text-xs text-muted-foreground">{t('reverseProxy.backendFields.protocol')}</label>
                <Select
                  value={protocol}
                  onValueChange={(v) =>
                    updateBackend(index, 'protocol', v as BackendProtocol)
                  }
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="http1">http1</SelectItem>
                    <SelectItem value="http2">http2</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
            {showH2cHint && (
              <p className="flex items-center gap-1.5 text-xs text-amber-500">
                <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
                {t('reverseProxy.backendFields.h2cHint')}
              </p>
            )}
          </div>
        );
      })}
      <Button type="button" variant="outline" size="sm" onClick={addBackend}>
        <Plus className="mr-2 h-4 w-4" />
        {t('reverseProxy.backendFields.addBackend')}
      </Button>
    </div>
  );
}
