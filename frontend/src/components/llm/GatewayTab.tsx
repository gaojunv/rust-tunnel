import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { useLlmGatewayConfig, useUpdateLlmGatewayConfig } from '@/api/hooks';

export default function GatewayTab() {
  const { t } = useTranslation();
  const { data: config, isLoading } = useLlmGatewayConfig();
  const updateMutation = useUpdateLlmGatewayConfig();
  const [enabled, setEnabled] = useState(false);
  const [openaiDomain, setOpenaiDomain] = useState('');
  const [anthropicDomain, setAnthropicDomain] = useState('');
  const [listen, setListen] = useState('0.0.0.0:443');
  const [tlsEnabled, setTlsEnabled] = useState(true);
  const [tlsAcme, setTlsAcme] = useState(false);

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled);
      setOpenaiDomain(config.openai_domain || '');
      setAnthropicDomain(config.anthropic_domain || '');
      setListen(config.listen || '0.0.0.0:443');
      setTlsEnabled(config.tls_enabled ?? true);
      setTlsAcme(config.tls_acme ?? false);
    }
  }, [config]);

  if (isLoading) return <div className="text-muted-foreground">{t('common.loading')}</div>;

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
        <Button
          onClick={() =>
            updateMutation.mutate({
              enabled,
              openai_domain: openaiDomain.trim() || null,
              anthropic_domain: anthropicDomain.trim() || null,
              listen,
              tls_enabled: tlsEnabled,
              tls_acme: tlsAcme,
            })
          }
          disabled={updateMutation.isPending}
        >
          {updateMutation.isPending ? t('common.saving') : t('common.save')}
        </Button>
      </CardContent>
    </Card>
  );
}
