import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { useTrojanConfig, useUpdateTrojanConfig, useAcmeStatus } from '@/api/hooks';
import { Info, ShieldCheck } from 'lucide-react';

export default function TrojanConfigCard() {
  const { t } = useTranslation();

  const CERT_SOURCE_LABEL_KEYS = {
    acme_exact: 'trojan.config.certSourceLabels.acme_exact',
    acme_wildcard: 'trojan.config.certSourceLabels.acme_wildcard',
    self_signed: 'trojan.config.certSourceLabels.self_signed',
  } as const;
  const { data: tjConfig, isLoading } = useTrojanConfig();
  const { data: acmeStatus } = useAcmeStatus();
  const updateTJ = useUpdateTrojanConfig();

  const [tjEnabled, setTjEnabled] = useState(false);
  const [tjPort, setTjPort] = useState('443');
  const [tjPassword, setTjPassword] = useState('');
  const [tjFallback, setTjFallback] = useState('127.0.0.1:80');
  const [tjDomain, setTjDomain] = useState('');

  useEffect(() => {
    if (tjConfig) {
      setTjEnabled(tjConfig.enabled ?? false);
      setTjPort(tjConfig.port?.toString() ?? '443');
      setTjFallback(tjConfig.fallback ?? '127.0.0.1:80');
      setTjDomain(tjConfig.domain ?? '');
      // Password not returned from API for security
    }
  }, [tjConfig]);

  const handleSaveTJ = () => {
    if (!tjPassword && !tjConfig?.enabled) {
      return; // Password required when enabling
    }
    updateTJ.mutate({
      enabled: tjEnabled,
      port: parseInt(tjPort, 10),
      ...(tjPassword && { password: tjPassword }),
      fallback: tjFallback || undefined,
      domain: tjDomain.trim(),
    });
  };

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">{t('common.loading')}</div>;
  }

  const certSourceLabel = tjConfig?.cert_source
    ? t(CERT_SOURCE_LABEL_KEYS[tjConfig.cert_source] ?? tjConfig.cert_source)
    : null;

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <ShieldCheck className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('trojan.config.title')}</CardTitle>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">{t('trojan.config.enable')}</span>
            <Switch checked={tjEnabled} onCheckedChange={setTjEnabled} />
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('trojan.config.port')}</label>
            <Input
              type="number"
              value={tjPort}
              onChange={(e) => setTjPort(e.target.value)}
              placeholder="443"
            />
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('trojan.config.domain')}</label>
            <Input
              value={tjDomain}
              onChange={(e) => setTjDomain(e.target.value)}
              placeholder="trojan.example.com"
            />
            <p className="text-xs text-muted-foreground">
              {t('trojan.config.domainHint')}
            </p>
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('trojan.config.password')}</label>
            <Input
              type="password"
              value={tjPassword}
              onChange={(e) => setTjPassword(e.target.value)}
              placeholder={tjConfig?.enabled ? '••••••••' : t('trojan.config.password')}
              autoComplete="new-password"
            />
            <p className="text-xs text-muted-foreground">
              {tjConfig?.enabled ? t('trojan.config.passwordHint.enabled') : t('trojan.config.passwordHint.disabled')}
            </p>
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('trojan.config.fallback')}</label>
            <Input
              value={tjFallback}
              onChange={(e) => setTjFallback(e.target.value)}
              placeholder="127.0.0.1:80"
            />
            <p className="text-xs text-muted-foreground">
              {t('trojan.config.fallbackHint')}
            </p>
          </div>
        </div>

        {/* Trojan TLS info */}
        <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
          <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
          <div className="text-sm text-muted-foreground">
            <p className="mb-1 font-medium text-foreground">{t('trojan.config.tls.title')}</p>
            {certSourceLabel && (
              <p className="mb-1">
                {t('trojan.config.tls.current')} <span className="font-medium text-foreground">{certSourceLabel}</span>
                {tjConfig?.cert_source === 'self_signed' && tjConfig.domain && (
                  <span className="text-amber-500">
                    {t('trojan.config.tls.acmeWarning')}
                  </span>
                )}
              </p>
            )}
            {tjConfig?.shared && (
              <p className="mb-1 text-emerald-500">
                {t('trojan.config.tls.sharedHint')}
              </p>
            )}
            <p>
              {t('trojan.config.tls.desc')}{' '}
              <a
                href="/acme"
                className="font-medium text-primary underline underline-offset-4 hover:text-primary/80"
              >
                ACME
              </a>
              .{' '}
              {!acmeStatus?.enabled && (
                <span className="text-amber-500">
                  {t('trojan.config.tls.acmeNotEnabled')}
                </span>
              )}
            </p>
          </div>
        </div>

        <Button onClick={handleSaveTJ} disabled={updateTJ.isPending}>
          {updateTJ.isPending ? t('common.saving') : t('trojan.config.save')}
        </Button>
      </CardContent>
    </Card>
  );
}
