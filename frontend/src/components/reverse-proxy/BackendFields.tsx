import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Plus, Trash2 } from 'lucide-react';
import type { Backend, BackendScheme, BackendProtocol } from '@/types';

interface BackendFieldsProps {
  backends: Backend[];
  onChange: (backends: Backend[]) => void;
}

export function BackendFields({ backends, onChange }: BackendFieldsProps) {
  const addBackend = () => {
    onChange([...backends, { addr: '', weight: 100 }]);
  };

  const removeBackend = (index: number) => {
    onChange(backends.filter((_, i) => i !== index));
  };

  const updateBackend = (
    index: number,
    field: keyof Backend,
    value: string | number,
  ) => {
    const updated = backends.map((b, i) =>
      i === index ? { ...b, [field]: value } : b
    );
    onChange(updated);
  };

  return (
    <div className="space-y-2">
      <label className="text-sm font-medium">Backend Servers</label>
      {backends.map((backend, index) => {
        const scheme = backend.scheme ?? 'http';
        const protocol = backend.protocol ?? 'http1';
        const showH2cHint = protocol === 'http2' && scheme === 'http';
        return (
          <div key={index} className="space-y-2 rounded-md border p-2">
            <div className="flex items-center gap-2">
              <Input
                value={backend.addr}
                onChange={(e) => updateBackend(index, 'addr', e.target.value)}
                placeholder="127.0.0.1:8080"
                className="flex-1"
              />
              <Input
                type="number"
                value={backend.weight}
                onChange={(e) =>
                  updateBackend(index, 'weight', parseInt(e.target.value, 10) || 100)
                }
                placeholder="Weight"
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
                <label className="text-xs text-muted-foreground">Scheme</label>
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
                <label className="text-xs text-muted-foreground">Protocol</label>
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
              <p className="text-xs text-yellow-600">
                h2c prior-knowledge: 后端需支持明文 HTTP/2
              </p>
            )}
          </div>
        );
      })}
      <Button type="button" variant="outline" size="sm" onClick={addBackend}>
        <Plus className="mr-2 h-4 w-4" />
        Add Backend
      </Button>
    </div>
  );
}
