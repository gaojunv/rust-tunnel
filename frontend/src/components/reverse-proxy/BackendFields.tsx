import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Plus, Trash2 } from 'lucide-react';
import type { Backend } from '@/types';

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

  const updateBackend = (index: number, field: keyof Backend, value: string | number) => {
    const updated = backends.map((b, i) =>
      i === index ? { ...b, [field]: value } : b
    );
    onChange(updated);
  };

  return (
    <div className="space-y-2">
      <label className="text-sm font-medium">Backend Servers</label>
      {backends.map((backend, index) => (
        <div key={index} className="flex items-center gap-2">
          <Input
            value={backend.addr}
            onChange={(e) => updateBackend(index, 'addr', e.target.value)}
            placeholder="127.0.0.1:8080"
            className="flex-1"
          />
          <Input
            type="number"
            value={backend.weight}
            onChange={(e) => updateBackend(index, 'weight', parseInt(e.target.value, 10) || 100)}
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
      ))}
      <Button type="button" variant="outline" size="sm" onClick={addBackend}>
        <Plus className="mr-2 h-4 w-4" />
        Add Backend
      </Button>
    </div>
  );
}
