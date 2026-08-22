import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  useLlmApiKeys,
  useCreateLlmApiKey,
  useToggleLlmApiKey,
  useBindLlmApiKey,
  useDeleteLlmApiKey,
  useLlmKbs,
} from '@/api/hooks';
import { getApiErrorMessage } from '@/api/client';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog, useConfirm } from './confirm';
import { Plus, Trash2, Copy, Check, AlertTriangle } from 'lucide-react';
import type { LlmApiKey, CreateApiKeyResponse } from '@/types';

// Radix Select 的 value 不能为空串，「无」用哨兵值，选中时映射回 null 解绑。
const NONE_KB = '__none__';

export default function ApiKeyTable() {
  const { t } = useTranslation();
  const { data: keys, isLoading } = useLlmApiKeys();
  const { data: kbs } = useLlmKbs();
  const createMutation = useCreateLlmApiKey();
  const toggleMutation = useToggleLlmApiKey();
  const bindMutation = useBindLlmApiKey();
  const deleteMutation = useDeleteLlmApiKey();
  const [newKeyName, setNewKeyName] = useState('');
  const [showNewKey, setShowNewKey] = useState<CreateApiKeyResponse | null>(null);
  const [copied, setCopied] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

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
          {actionError && (
            <div className="mb-3 flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <AlertTriangle className="h-4 w-4 shrink-0" />
              {actionError}
            </div>
          )}
          {isLoading ? (
            <div className="space-y-2">
              <Skeleton className="h-12 w-full rounded" />
              <Skeleton className="h-12 w-full rounded" />
            </div>
          ) : keys?.length === 0 ? <div className="text-muted-foreground text-sm">{t('llm.apiKeys.empty')}</div> : (
            <div className="space-y-1">
              {keys?.map((k: LlmApiKey) => {
                const boundKb = k.kb_id ? kbs?.find((kb) => kb.id === k.kb_id) : undefined;
                // 悬空引用：kb_id 存在但知识库已被删除
                const dangling = !!k.kb_id && !boundKb;
                const binding = bindMutation.isPending && bindMutation.variables?.id === k.id;
                return (
                  <div key={k.id} className="flex flex-wrap items-center justify-between gap-2 text-sm py-2 border-b">
                    <div>
                      <div className="flex items-center gap-2">
                        <span className="font-medium">{k.name || t('llm.apiKeys.unnamed')}</span>
                        {boundKb ? (
                          <Badge variant="outline" className="shrink-0">{boundKb.name}</Badge>
                        ) : dangling ? (
                          <Badge variant="secondary" className="shrink-0">{t('llm.apiKeys.kbDeleted')}</Badge>
                        ) : null}
                      </div>
                      <code className="text-xs text-muted-foreground">{k.key_prefix}</code>
                    </div>
                    <div className="flex items-center gap-2">
                      <Select
                        value={k.kb_id ?? NONE_KB}
                        disabled={binding}
                        onValueChange={(v) => {
                          setActionError(null);
                          bindMutation.mutate(
                            { id: k.id, kbId: v === NONE_KB ? null : v },
                            {
                              onError: (err) => {
                                setActionError(t('llm.apiKeys.bindError', { error: getApiErrorMessage(err) }));
                              },
                            },
                          );
                        }}
                      >
                        <SelectTrigger aria-label={t('llm.apiKeys.kbBind')} className="w-44">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value={NONE_KB}>{t('llm.apiKeys.kbNone')}</SelectItem>
                          {(kbs ?? []).map((kb) => (
                            <SelectItem key={kb.id} value={kb.id}>
                              {kb.name}
                            </SelectItem>
                          ))}
                          {dangling && (
                            <SelectItem value={k.kb_id ?? NONE_KB}>{t('llm.apiKeys.kbDeleted')}</SelectItem>
                          )}
                        </SelectContent>
                      </Select>
                      <Switch checked={k.enabled} onCheckedChange={(v) => toggleMutation.mutate({ id: k.id, enabled: v })} />
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          confirm(
                            { title: t('common.confirm'), description: t('llm.apiKeys.revokeConfirm') },
                            () => {
                              setActionError(null);
                              deleteMutation.mutate(k.id, {
                                onError: (err) => setActionError(t('common.saveError', { error: getApiErrorMessage(err) })),
                              });
                            },
                          )
                        }
                      >
                        <Trash2 className="w-3 h-3 text-destructive" />
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </CardContent>
      </Card>
      <ConfirmDialog open={confirmOpen} payload={confirmPayload} onConfirm={confirmAndClose} onCancel={cancelConfirm} variant="destructive" />
    </div>
  );
}
