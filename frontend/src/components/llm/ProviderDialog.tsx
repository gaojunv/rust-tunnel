import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
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

  const existing = providerId ? providers?.find((p) => p.id === providerId) : null;

  useEffect(() => {
    if (open) {
      if (existing) { setName(existing.name); setProviderType(existing.provider_type); setBaseUrl(existing.base_url); setApiKey(''); setAnthropicBaseUrl(existing.anthropic_base_url || ''); }
      else { setName(''); setProviderType('deepseek'); setBaseUrl('https://api.deepseek.com'); setApiKey(''); setAnthropicBaseUrl(''); }
    }
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
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>{t('common.cancel')}</Button>
          <Button onClick={() => {
            const req = { name, provider_type: providerType, base_url: baseUrl, api_key: apiKey, anthropic_base_url: anthropicBaseUrl || null };
            if (existing) { updateMutation.mutate({ id: existing.id, ...req }, { onSuccess: onClose }); }
            else { createMutation.mutate(req, { onSuccess: onClose }); }
          }} disabled={createMutation.isPending || updateMutation.isPending}>{t('common.save')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
