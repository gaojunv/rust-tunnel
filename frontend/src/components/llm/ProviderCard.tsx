import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from '@/components/ui/select';
import { useProviderModels, useAddModel, useUpdateModel, useDeleteModel, useToggleLlmProvider, useDeleteLlmProvider } from '@/api/hooks';
import type { LlmProvider, LlmModel } from '@/types';
import { CONTEXT_LIMIT_OPTIONS, parseContextLimit, mergeContextLimit } from './contextLimit';
import type { ContextLimitTier } from './contextLimit';
import { parseUpstreamProtocol, mergeUpstreamProtocol } from './upstreamProtocol';
import type { UpstreamProtocol } from './upstreamProtocol';
import { ConfirmDialog, useConfirm } from './confirm';
import { Trash2, Plus, Edit3, ChevronDown, ChevronRight, Check, X } from 'lucide-react';

interface Props { provider: LlmProvider; onEdit: () => void; }

/** 单个模型行：展示 + 别名/标签的内联编辑 */
function ModelRow({ model, confirm }: { model: LlmModel; confirm: ReturnType<typeof useConfirm>['confirm'] }) {
  const { t } = useTranslation();
  const updateModelMutation = useUpdateModel();
  const deleteModelMutation = useDeleteModel();
  const [editing, setEditing] = useState(false);
  const [alias, setAlias] = useState(model.alias);
  const [tags, setTags] = useState(model.tags?.join(', ') ?? '');
  const [contextLimitTier, setContextLimitTier] = useState<ContextLimitTier>(parseContextLimit(model.extra_config));
  const [upstreamProtocol, setUpstreamProtocol] = useState<UpstreamProtocol>(parseUpstreamProtocol(model.extra_config));

  const save = () => {
    const merged = mergeUpstreamProtocol(
      mergeContextLimit(model.extra_config, contextLimitTier),
      upstreamProtocol,
    );
    updateModelMutation.mutate(
      {
        id: model.id,
        model_name: model.model_name,
        alias: alias.trim(),
        tags: tags.split(',').map((t) => t.trim()).filter(Boolean),
        extra_config: merged,
      },
      { onSuccess: () => setEditing(false) },
    );
  };

  if (editing) {
    return (
      <div className="flex items-center gap-2 text-sm py-1 border-b">
        <span className="min-w-0 truncate font-mono">{model.model_name}</span>
        <Input placeholder={t('llm.providerCard.aliasPlaceholder')} value={alias} onChange={(e) => setAlias(e.target.value)} className="h-7 w-32" />
        <Input placeholder={t('llm.providerCard.tagsPlaceholder')} value={tags} onChange={(e) => setTags(e.target.value)} className="h-7 flex-1" />
        <Select value={contextLimitTier} onValueChange={(v) => setContextLimitTier(v as ContextLimitTier)}>
          <SelectTrigger className="h-7 w-28" aria-label={t('llm.providerCard.contextLimitLabel')}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {CONTEXT_LIMIT_OPTIONS.map((opt) => (
              <SelectItem key={opt.value} value={opt.value}>
                {opt.value.toUpperCase()}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select value={upstreamProtocol} onValueChange={(v) => setUpstreamProtocol(v as UpstreamProtocol)}>
          <SelectTrigger className="h-7 w-36" aria-label={t('llm.providerCard.upstreamProtocolLabel')}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="chat_completions">{t('llm.providerCard.protocolChatCompletions')}</SelectItem>
            <SelectItem value="responses">{t('llm.providerCard.protocolResponses')}</SelectItem>
          </SelectContent>
        </Select>
        <Button variant="ghost" size="icon" aria-label={t('common.save')} onClick={save} disabled={updateModelMutation.isPending}>
          <Check className="w-3 h-3" />
        </Button>
        <Button variant="ghost" size="icon" aria-label={t('common.cancel')} onClick={() => { setContextLimitTier(parseContextLimit(model.extra_config)); setUpstreamProtocol(parseUpstreamProtocol(model.extra_config)); setEditing(false); }}>
          <X className="w-3 h-3" />
        </Button>
      </div>
    );
  }

  return (
    <div className="flex items-center justify-between gap-2 text-sm py-1 border-b">
      <div className="min-w-0">
        <span className="truncate font-mono">{model.model_name}</span>
        {model.alias && <span className="text-muted-foreground ml-2">({model.alias})</span>}
        {model.tags?.map((t) => <Badge key={t} variant="outline" className="ml-1 text-xs">{t}</Badge>)}
        {parseUpstreamProtocol(model.extra_config) === 'responses' && <Badge variant="secondary" className="ml-1 text-xs">Responses</Badge>}
      </div>
      <div className="flex items-center">
        <Button variant="ghost" size="icon" aria-label={t('llm.providerCard.editModel')} onClick={() => { setAlias(model.alias); setTags(model.tags?.join(', ') ?? ''); setContextLimitTier(parseContextLimit(model.extra_config)); setUpstreamProtocol(parseUpstreamProtocol(model.extra_config)); setEditing(true); }}>
          <Edit3 className="w-3 h-3" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          aria-label={t('llm.providerCard.deleteModel')}
          onClick={() =>
            confirm(
              { title: t('common.confirm'), description: t('llm.providerCard.deleteModelConfirm', { name: model.model_name }) },
              () => deleteModelMutation.mutate(model.id),
            )
          }
        >
          <Trash2 className="w-3 h-3 text-destructive" />
        </Button>
      </div>
    </div>
  );
}

export default function ProviderCard({ provider, onEdit }: Props) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const { data: models } = useProviderModels(expanded ? provider.id : '');
  const addModelMutation = useAddModel();
  const toggleMutation = useToggleLlmProvider();
  const deleteMutation = useDeleteLlmProvider();
  const [newModelName, setNewModelName] = useState('');
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="icon" aria-label={expanded ? t('llm.providerCard.collapse') : t('llm.providerCard.expand')} onClick={() => setExpanded(!expanded)}>
            {expanded ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
          </Button>
          <CardTitle className="text-base">{provider.name}</CardTitle>
          <Badge variant="secondary">{provider.provider_type}</Badge>
        </div>
        <div className="flex items-center gap-2">
          <Switch checked={provider.enabled} onCheckedChange={(v) => toggleMutation.mutate({ id: provider.id, enabled: v })} />
          <Button variant="ghost" size="icon" aria-label={t('llm.providerCard.editProvider')} onClick={onEdit}><Edit3 className="w-4 h-4" /></Button>
          <Button
            variant="ghost"
            size="icon"
            aria-label={t('llm.providerCard.deleteProvider')}
            onClick={() =>
              confirm(
                { title: t('common.confirm'), description: t('llm.providerCard.deleteProviderConfirm') },
                () => deleteMutation.mutate(provider.id),
              )
            }
          >
            <Trash2 className="w-4 h-4 text-destructive" />
          </Button>
        </div>
      </CardHeader>
      {expanded && (
        <CardContent>
          <div className="space-y-2">
            <div className="text-xs text-muted-foreground">{t('llm.providerCard.baseUrl', { url: provider.base_url })}</div>
            {provider.anthropic_base_url && <div className="text-xs text-muted-foreground">{t('llm.providerCard.anthropicUrl', { url: provider.anthropic_base_url })}</div>}
          </div>
          <div className="space-y-2 mt-3">
            <div className="flex gap-2">
              <Input placeholder={t('llm.providerCard.modelNamePlaceholder')} value={newModelName} onChange={(e) => setNewModelName(e.target.value)} className="flex-1" />
              <Button size="sm" onClick={() => { if (newModelName.trim()) { addModelMutation.mutate({ providerId: provider.id, model_name: newModelName.trim() }); setNewModelName(''); } }} disabled={addModelMutation.isPending}>
                <Plus className="w-4 h-4" /> {t('llm.providerCard.addModel')}
              </Button>
            </div>
            {models?.map((m: LlmModel) => (
              <ModelRow key={m.id} model={m} confirm={confirm} />
            ))}
          </div>
        </CardContent>
      )}
      <ConfirmDialog open={confirmOpen} payload={confirmPayload} onConfirm={confirmAndClose} onCancel={cancelConfirm} variant="destructive" />
    </Card>
  );
}
