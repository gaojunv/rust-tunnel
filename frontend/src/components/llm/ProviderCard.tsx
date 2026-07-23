import { useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { useProviderModels, useAddModel, useUpdateModel, useDeleteModel, useToggleLlmProvider, useDeleteLlmProvider } from '@/api/hooks';
import type { LlmProvider, LlmModel } from '@/types';
import { Trash2, Plus, Edit3, ChevronDown, ChevronRight, Check, X } from 'lucide-react';

interface Props { provider: LlmProvider; onEdit: () => void; }

/** 单个模型行：展示 + 别名/标签的内联编辑 */
function ModelRow({ model }: { model: LlmModel }) {
  const updateModelMutation = useUpdateModel();
  const deleteModelMutation = useDeleteModel();
  const [editing, setEditing] = useState(false);
  const [alias, setAlias] = useState(model.alias);
  const [tags, setTags] = useState(model.tags?.join(', ') ?? '');

  const save = () => {
    updateModelMutation.mutate(
      {
        id: model.id,
        model_name: model.model_name,
        alias: alias.trim(),
        tags: tags.split(',').map((t) => t.trim()).filter(Boolean),
      },
      { onSuccess: () => setEditing(false) },
    );
  };

  if (editing) {
    return (
      <div className="flex items-center gap-2 text-sm py-1 border-b">
        <span className="font-mono">{model.model_name}</span>
        <Input placeholder="Alias" value={alias} onChange={(e) => setAlias(e.target.value)} className="h-7 w-32" />
        <Input placeholder="Tags (comma separated)" value={tags} onChange={(e) => setTags(e.target.value)} className="h-7 flex-1" />
        <Button variant="ghost" size="icon" onClick={save} disabled={updateModelMutation.isPending}>
          <Check className="w-3 h-3" />
        </Button>
        <Button variant="ghost" size="icon" onClick={() => setEditing(false)}>
          <X className="w-3 h-3" />
        </Button>
      </div>
    );
  }

  return (
    <div className="flex items-center justify-between text-sm py-1 border-b">
      <div>
        <span className="font-mono">{model.model_name}</span>
        {model.alias && <span className="text-muted-foreground ml-2">({model.alias})</span>}
        {model.tags?.map((t) => <Badge key={t} variant="outline" className="ml-1 text-xs">{t}</Badge>)}
      </div>
      <div className="flex items-center">
        <Button variant="ghost" size="icon" onClick={() => { setAlias(model.alias); setTags(model.tags?.join(', ') ?? ''); setEditing(true); }}>
          <Edit3 className="w-3 h-3" />
        </Button>
        <Button variant="ghost" size="icon" onClick={() => { if (confirm(`Delete model "${model.model_name}"?`)) deleteModelMutation.mutate(model.id); }}>
          <Trash2 className="w-3 h-3 text-destructive" />
        </Button>
      </div>
    </div>
  );
}

export default function ProviderCard({ provider, onEdit }: Props) {
  const [expanded, setExpanded] = useState(false);
  const { data: models } = useProviderModels(expanded ? provider.id : '');
  const addModelMutation = useAddModel();
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
            <div className="text-xs text-muted-foreground">Base URL: {provider.base_url}</div>
            {provider.anthropic_base_url && <div className="text-xs text-muted-foreground">Anthropic URL: {provider.anthropic_base_url}</div>}
          </div>
          <div className="space-y-2 mt-3">
            <div className="flex gap-2">
              <Input placeholder="Model name (e.g., deepseek-chat)" value={newModelName} onChange={(e) => setNewModelName(e.target.value)} className="flex-1" />
              <Button size="sm" onClick={() => { if (newModelName.trim()) { addModelMutation.mutate({ providerId: provider.id, model_name: newModelName.trim() }); setNewModelName(''); } }} disabled={addModelMutation.isPending}>
                <Plus className="w-4 h-4" /> Add
              </Button>
            </div>
            {models?.map((m: LlmModel) => <ModelRow key={m.id} model={m} />)}
          </div>
        </CardContent>
      )}
    </Card>
  );
}
