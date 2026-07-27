import { useState, useEffect } from 'react';
import { useTranslation, Trans } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useSettings, useAcmeStatus } from '@/api/hooks';
import { Info, Server, Lock, ArrowLeftRight } from 'lucide-react';

const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

export default function GeneralTab() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useSettings();
  const { data: acmeStatus } = useAcmeStatus();

  const [apiTls, setApiTls] = useState(false);
  const [apiDomain, setApiDomain] = useState('');

  useEffect(() => {
    if (settings) {
      setApiTls(settings.api_tls ?? false);
      setApiDomain(settings.api_domain ?? '');
    }
  }, [settings]);

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">{t('common.loading')}</div>;
  }

  return (
    <div className="space-y-6">
      {/* Log Level (read-only display) */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Server className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.general.system')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('settings.general.logLevel')}</label>
            <Select value={settings?.log_level ?? 'info'} disabled>
              <SelectTrigger className="w-48">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {LOG_LEVELS.map((l) => (
                  <SelectItem key={l} value={l}>
                    {l}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground">
              {t('settings.general.logLevelReadonly')}
            </p>
          </div>
        </CardContent>
      </Card>

      {/* API Server TLS */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Lock className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.general.apiTls')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <label className="text-sm font-medium">{t('settings.general.apiTlsEnable')}</label>
              <p className="text-xs text-muted-foreground">
                {t('settings.general.apiTlsEnableDesc')}
              </p>
            </div>
            <Switch
              checked={apiTls}
              onCheckedChange={setApiTls}
              disabled
            />
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">{t('settings.general.apiDomain')}</label>
            <Input
              value={apiDomain}
              onChange={(e) => setApiDomain(e.target.value)}
              placeholder={t('settings.general.apiDomainPlaceholder')}
              disabled
            />
            <p className="text-xs text-muted-foreground">
              {t('settings.general.apiDomainDesc')}
            </p>
          </div>

          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p className="mb-1 font-medium text-foreground">{t('settings.general.configViaFile')}</p>
              <p>
                {t('settings.general.configViaFileDesc')}
              </p>
              {!acmeStatus?.enabled && (
                <p className="mt-2 text-amber-500">
                  ⚠ {t('settings.general.acmeWarning')}
                </p>
              )}
            </div>
          </div>
        </CardContent>
      </Card>

      {/* Reverse Proxy */}
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <ArrowLeftRight className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.general.reverseProxy')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p>
                <Trans
                  i18nKey="settings.general.reverseProxyDesc"
                  components={[
                    <a
                      href="/proxy"
                      className="font-medium text-primary underline underline-offset-4 hover:text-primary/80"
                    >
                      Reverse Proxy
                    </a>,
                  ]}
                />
              </p>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
