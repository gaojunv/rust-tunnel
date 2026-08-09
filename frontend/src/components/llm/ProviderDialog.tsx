import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { useCreateLlmProvider, useUpdateLlmProvider, useLlmProviders } from '@/api/hooks';
import type { ProviderType } from '@/types';

interface Props { open: boolean; onClose: () => void; providerId: string | null; }

const TYPES: { value: ProviderType; label: string; defaultUrl: string }[] = [
  { value: 'deepseek', label: 'DeepSeek', defaultUrl: 'https://api.deepseek.com' },
  { value: 'volcengine', label: '火山方舟', defaultUrl: 'https://ark.cn-beijing.volces.com/api/v3' },
  { value: 'kimi', label: 'Kimi (Moonshot)', defaultUrl: 'https://api.moonshot.cn' },
  { value: 'mimo', label: 'Mimo', defaultUrl: '' },
];

/** 从 extra_config JSON 里读出开关状态；非法/缺省视为关闭。 */
function parseCompat(extraConfig?: string | null): boolean {
  if (!extraConfig) return false;
  try { return (JSON.parse(extraConfig) as { compat_tool_history?: boolean }).compat_tool_history === true; }
  catch { return false; }
}

/** 把开关状态合并回 extra_config JSON，保留已有其他键。 */
function mergeCompat(extraConfig: string | null | undefined, compat: boolean): string | null {
  let obj: Record<string, unknown> = {};
  if (extraConfig) { try { obj = JSON.parse(extraConfig) as Record<string, unknown>; } catch { obj = {}; } }
  if (compat) obj.compat_tool_history = true; else delete obj.compat_tool_history;
  return Object.keys(obj).length > 0 ? JSON.stringify(obj) : null;
}

export default function ProviderDialog({ open, onClose, providerId }: Props) {
  const { t } = useTranslation();
  const { data: providers } = useLlmProviders();
  const createMutation = useCreateLlmProvider();
  const updateMutation = useUpdateLlmProvider();
  const [name, setName] = useState('');
  const [providerType, setProviderType] = useState<ProviderType>('deepseek');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [anthropicBaseUrl, setAnthropicBaseUrl] = useState('');
  const [compatToolHistory, setCompatToolHistory] = useState(false);

  const existing = providerId ? providers?.find((p) => p.id === providerId) : null;

  // Initialize the form exactly once per open cycle. `existing` is an object
  // reference inside the live `llm-providers` query array, so it changes on every
  // refetch (window focus, staleTime expiry); re-running the init on those changes
  // would clobber in-progress edits. Mirrors KbDialog's initRef guard.
  const initRef = useRef(false);

  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;

    if (existing) { setName(existing.name); setProviderType(existing.provider_type); setBaseUrl(existing.base_url); setApiKey(''); setAnthropicBaseUrl(existing.anthropic_base_url || ''); setCompatToolHistory(parseCompat(existing.extra_config)); }
    else { setName(''); setProviderType('deepseek'); setBaseUrl('https://api.deepseek.com'); setApiKey(''); setAnthropicBaseUrl(''); setCompatToolHistory(false); }
  }, [open, existing]);

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent>
        <DialogHeader><DialogTitle>{existing ? t('llm.providerDialog.editTitle') : t('llm.providerDialog.addTitle')}</DialogTitle></DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2"><Label>{t('llm.providerDialog.name')}</Label><Input value={name} onChange={(e) => setName(e.target.value)} placeholder={t('llm.providerDialog.namePlaceholder')} /></div>
          <div className="space-y-2">
            <Label>{t('llm.providerDialog.providerType')}</Label>
            <Select value={providerType} onValueChange={(v) => { setProviderType(v as ProviderType); const info = TYPES.find((t) => t.value === v); if (info) setBaseUrl(info.defaultUrl); }}>
              <SelectTrigger><SelectValue /></SelectTrigger>
              <SelectContent>{TYPES.map((pt) => <SelectItem key={pt.value} value={pt.value}>{pt.label}</SelectItem>)}</SelectContent>
            </Select>
          </div>
          <div className="space-y-2"><Label>{t('llm.providerDialog.baseUrl')}</Label><Input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} /></div>
          <div className="space-y-2"><Label>{t('llm.providerDialog.anthropicBaseUrl')}</Label><Input value={anthropicBaseUrl} onChange={(e) => setAnthropicBaseUrl(e.target.value)} placeholder={t('llm.providerDialog.anthropicBaseUrlPlaceholder')} /></div>
          <div className="space-y-2"><Label>{t('llm.providerDialog.apiKey')}</Label><Input type="password" value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder={existing ? t('llm.providerDialog.apiKeyPlaceholderEdit') : t('llm.providerDialog.apiKeyPlaceholderNew')} /></div>
          <div className="flex items-center justify-between space-x-2">
            <Label className="flex flex-col space-y-1">
              <span>{t('llm.providerDialog.compatToolHistory')}</span>
              <span className="font-normal text-xs text-muted-foreground">{t('llm.providerDialog.compatToolHistoryHint')}</span>
            </Label>
            <Switch checked={compatToolHistory} onCheckedChange={setCompatToolHistory} />
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>{t('common.cancel')}</Button>
          <Button onClick={() => {
            const req = { name, provider_type: providerType, base_url: baseUrl, api_key: apiKey, anthropic_base_url: anthropicBaseUrl || null, extra_config: mergeCompat(existing?.extra_config, compatToolHistory) };
            if (existing) { updateMutation.mutate({ id: existing.id, ...req }, { onSuccess: onClose }); }
            else { createMutation.mutate(req, { onSuccess: onClose }); }
          }} disabled={createMutation.isPending || updateMutation.isPending}>{t('common.save')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
