import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { Skeleton } from '@/components/ui/skeleton';
import { AlertTriangle, Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useLlmGatewayConfig, useUpdateLlmGatewayConfig } from '@/api/hooks';

const DEFAULT_LISTEN = '0.0.0.0:443';

export default function GatewayTab() {
  const { t } = useTranslation();
  const { data: config, isLoading } = useLlmGatewayConfig();
  const updateMutation = useUpdateLlmGatewayConfig();
  const [enabled, setEnabled] = useState(false);
  const [openaiDomain, setOpenaiDomain] = useState('');
  const [anthropicDomain, setAnthropicDomain] = useState('');
  const [listen, setListen] = useState(DEFAULT_LISTEN);
  const [tlsEnabled, setTlsEnabled] = useState(true);
  const [tlsAcme, setTlsAcme] = useState(false);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  // initRef：仅在配置首次加载时回填表单。`config` 是 react-query 缓存对象，
  // 窗口聚焦/staleTime 过期引发的 refetch 会换新引用；若不设守卫，那些 refetch
  // 会覆盖用户正在编辑的输入（仿 ProviderDialog/KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!config || initRef.current) return;
    initRef.current = true;
    setEnabled(config.enabled);
    setOpenaiDomain(config.openai_domain || '');
    setAnthropicDomain(config.anthropic_domain || '');
    setListen(config.listen || DEFAULT_LISTEN);
    setTlsEnabled(config.tls_enabled ?? true);
    setTlsAcme(config.tls_acme ?? false);
  }, [config]);

  // 脏检查：与服务端配置逐字段比对，无改动时禁用保存按钮。
  const dirty =
    !!config &&
    (enabled !== config.enabled ||
      openaiDomain.trim() !== (config.openai_domain || '') ||
      anthropicDomain.trim() !== (config.anthropic_domain || '') ||
      listen !== (config.listen || DEFAULT_LISTEN) ||
      tlsEnabled !== (config.tls_enabled ?? true) ||
      tlsAcme !== (config.tls_acme ?? false));

  const submit = () => {
    setSaveMsg(null);
    setSaveError(null);
    updateMutation.mutate(
      {
        enabled,
        openai_domain: openaiDomain.trim() || null,
        anthropic_domain: anthropicDomain.trim() || null,
        listen,
        tls_enabled: tlsEnabled,
        tls_acme: tlsAcme,
      },
      {
        // 保存成功后放开 initRef，让失效后重来的服务端值重新成为基线，
        // 使脏检查回到「无改动」状态。
        onSuccess: () => {
          initRef.current = false;
          setSaveMsg(t('llm.gateway.saved'));
        },
        onError: (err) => {
          setSaveError(t('llm.gateway.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  if (isLoading) {
    return (
      <Card>
        <CardContent className="space-y-3 pt-6">
          <Skeleton className="h-6 w-full rounded" />
          <Skeleton className="h-10 w-full rounded" />
          <Skeleton className="h-10 w-full rounded" />
          <Skeleton className="h-10 w-full rounded" />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader><CardTitle>{t('llm.gateway.title')}</CardTitle></CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between">
          <Label>{t('llm.gateway.enableGateway')}</Label>
          <Switch checked={enabled} onCheckedChange={setEnabled} />
        </div>
        <div className="space-y-2">
          <Label>{t('llm.gateway.openaiDomain')}</Label>
          <Input
            value={openaiDomain}
            onChange={(e) => setOpenaiDomain(e.target.value)}
            placeholder="openai.example.com"
            disabled={!enabled}
          />
          <p className="text-xs text-muted-foreground">
            {t('llm.gateway.openaiDomainHint')}
          </p>
        </div>
        <div className="space-y-2">
          <Label>{t('llm.gateway.anthropicDomain')}</Label>
          <Input
            value={anthropicDomain}
            onChange={(e) => setAnthropicDomain(e.target.value)}
            placeholder="anthropic.example.com"
            disabled={!enabled}
          />
          <p className="text-xs text-muted-foreground">
            {t('llm.gateway.anthropicDomainHint')}
          </p>
        </div>
        <div className="space-y-2">
          <Label>{t('llm.gateway.listenAddress')}</Label>
          <Input value={listen} onChange={(e) => setListen(e.target.value)} placeholder="0.0.0.0:443" disabled={!enabled} />
        </div>
        <div className="flex items-center justify-between">
          <Label>{t('llm.gateway.tls')}</Label>
          <Switch checked={tlsEnabled} onCheckedChange={setTlsEnabled} disabled={!enabled} />
        </div>
        {tlsEnabled && (
          <div className="flex items-center justify-between">
            <Label>{t('llm.gateway.acmeAutoRenew')}</Label>
            <Switch checked={tlsAcme} onCheckedChange={setTlsAcme} disabled={!enabled} />
          </div>
        )}
        {saveMsg && (
          <p className="text-sm text-emerald-600 dark:text-emerald-400">{saveMsg}</p>
        )}
        {saveError && (
          <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            <AlertTriangle className="h-4 w-4 shrink-0" />
            {saveError}
          </div>
        )}
        <div className="flex items-center gap-3">
          <Button onClick={submit} disabled={updateMutation.isPending || !dirty}>
            {updateMutation.isPending && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {updateMutation.isPending ? t('common.saving') : t('common.save')}
          </Button>
          {dirty && !updateMutation.isPending && (
            <span className="text-xs text-muted-foreground">{t('llm.gateway.unsaved')}</span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
