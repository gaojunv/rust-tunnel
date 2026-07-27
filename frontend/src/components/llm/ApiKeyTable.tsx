import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { useLlmApiKeys, useCreateLlmApiKey, useToggleLlmApiKey, useDeleteLlmApiKey } from '@/api/hooks';
import { Plus, Trash2, Copy, Check } from 'lucide-react';
import type { LlmApiKey, CreateApiKeyResponse } from '@/types';

export default function ApiKeyTable() {
  const { t } = useTranslation();
  const { data: keys, isLoading } = useLlmApiKeys();
  const createMutation = useCreateLlmApiKey();
  const toggleMutation = useToggleLlmApiKey();
  const deleteMutation = useDeleteLlmApiKey();
  const [newKeyName, setNewKeyName] = useState('');
  const [showNewKey, setShowNewKey] = useState<CreateApiKeyResponse | null>(null);
  const [copied, setCopied] = useState(false);

  return (
    <div className="space-y-4">
      {showNewKey && (
        <Card className="border-yellow-500 bg-yellow-50 dark:bg-yellow-950">
          <CardContent className="pt-4 space-y-2">
            <p className="text-sm font-semibold">{t('llm.apiKeys.newKeyNotice')}</p>
            <div className="flex gap-2">
              <code className="flex-1 p-2 bg-background rounded text-sm break-all">{showNewKey.key}</code>
              <Button variant="outline" size="icon" onClick={() => { navigator.clipboard.writeText(showNewKey.key); setCopied(true); setTimeout(() => setCopied(false), 2000); }}>
                {copied ? <Check className="w-4 h-4 text-green-500" /> : <Copy className="w-4 h-4" />}
              </Button>
            </div>
            <Button variant="link" className="p-0" onClick={() => setShowNewKey(null)}>{t('llm.apiKeys.dismiss')}</Button>
          </CardContent>
        </Card>
      )}
      <Card>
        <CardHeader><CardTitle>{t('llm.apiKeys.title')}</CardTitle></CardHeader>
        <CardContent>
          <div className="flex gap-2 mb-4">
            <Input placeholder={t('llm.apiKeys.keyNamePlaceholder')} value={newKeyName} onChange={(e) => setNewKeyName(e.target.value)} className="flex-1" />
            <Button onClick={() => { if (newKeyName.trim()) { createMutation.mutate(newKeyName.trim(), { onSuccess: (data) => { setShowNewKey(data); setNewKeyName(''); } }); } }} disabled={createMutation.isPending}>
              <Plus className="w-4 h-4 mr-2" /> {t('llm.apiKeys.generateKey')}
            </Button>
          </div>
          {isLoading ? <div className="text-muted-foreground">{t('common.loading')}</div> : keys?.length === 0 ? <div className="text-muted-foreground text-sm">{t('llm.apiKeys.empty')}</div> : (
            <div className="space-y-1">
              {keys?.map((k: LlmApiKey) => (
                <div key={k.id} className="flex items-center justify-between text-sm py-2 border-b">
                  <div>
                    <div className="font-medium">{k.name || t('llm.apiKeys.unnamed')}</div>
                    <code className="text-xs text-muted-foreground">{k.key_prefix}</code>
                  </div>
                  <div className="flex items-center gap-2">
                    <Switch checked={k.enabled} onCheckedChange={(v) => toggleMutation.mutate({ id: k.id, enabled: v })} />
                    <Button variant="ghost" size="icon" onClick={() => { if (confirm(t('llm.apiKeys.revokeConfirm'))) deleteMutation.mutate(k.id); }}>
                      <Trash2 className="w-3 h-3 text-destructive" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
