import { useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { useProviderModels, useAddModel, useDeleteModel, useToggleLlmProvider, useDeleteLlmProvider } from '@/api/hooks';
import type { LlmProvider, LlmModel } from '@/types';
import { Trash2, Plus, Edit3, ChevronDown, ChevronRight } from 'lucide-react';

interface Props { provider: LlmProvider; onEdit: () => void; }

export default function ProviderCard({ provider, onEdit }: Props) {
  const [expanded, setExpanded] = useState(false);
  const { data: models } = useProviderModels(expanded ? provider.id : '');
  const addModelMutation = useAddModel();
  const deleteModelMutation = useDeleteModel();
  const toggleMutation = useToggleLlmProvider();
  const deleteMutation = useDeleteLlmProvider();
  const [newModelName, setNewModelName] = useState('');

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="icon" onClick={() => setExpanded(!expanded)}>
            {expanded ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
          </Button>
          <CardTitle className="text-base">{provider.name}</CardTitle>
          <Badge variant="secondary">{provider.provider_type}</Badge>
        </div>
        <div className="flex items-center gap-2">
          <Switch checked={provider.enabled} onCheckedChange={(v) => toggleMutation.mutate({ id: provider.id, enabled: v })} />
          <Button variant="ghost" size="icon" onClick={onEdit}><Edit3 className="w-4 h-4" /></Button>
          <Button variant="ghost" size="icon" onClick={() => { if (confirm('Delete this provider and all its models?')) deleteMutation.mutate(provider.id); }}>
            <Trash2 className="w-4 h-4 text-destructive" />
          </Button>
        </div>
      </CardHeader>
      {expanded && (
        <CardContent>
          <div className="space-y-2">
            <div className="flex gap-2">
              <Input placeholder="Model name (e.g., deepseek-chat)" value={newModelName} onChange={(e) => setNewModelName(e.target.value)} className="flex-1" />
              <Button size="sm" onClick={() => { if (newModelName.trim()) { addModelMutation.mutate({ providerId: provider.id, model_name: newModelName.trim() }); setNewModelName(''); } }} disabled={addModelMutation.isPending}>
                <Plus className="w-4 h-4" /> Add
              </Button>
            </div>
            {models?.map((m: LlmModel) => (
              <div key={m.id} className="flex items-center justify-between text-sm py-1 border-b">
                <div>
                  <span className="font-mono">{m.model_name}</span>
                  {m.alias && <span className="text-muted-foreground ml-2">({m.alias})</span>}
                  {m.tags?.map((t) => <Badge key={t} variant="outline" className="ml-1 text-xs">{t}</Badge>)}
                </div>
                <Button variant="ghost" size="icon" onClick={() => { if (confirm(`Delete model "${m.model_name}"?`)) deleteModelMutation.mutate(m.id); }}>
                  <Trash2 className="w-3 h-3 text-destructive" />
                </Button>
              </div>
            ))}
          </div>
        </CardContent>
      )}
    </Card>
  );
}
