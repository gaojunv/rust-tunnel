import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useShadowsocksConfig, useUpdateShadowsocksConfig } from '@/api/hooks';
import { Shield } from 'lucide-react';

export default function ShadowsocksConfigCard() {
  const { t } = useTranslation();
  const { data: ssConfig, isLoading } = useShadowsocksConfig();
  const updateSS = useUpdateShadowsocksConfig();

  const [ssEnabled, setSsEnabled] = useState(false);
  const [ssPort, setSsPort] = useState('8388');
  const [ssCipher, setSsCipher] = useState('aes-256-gcm');
  const [ssPassword, setSsPassword] = useState('');

  useEffect(() => {
    if (ssConfig) {
      setSsEnabled(ssConfig.enabled ?? false);
      setSsPort(ssConfig.port?.toString() ?? '8388');
      setSsCipher(ssConfig.cipher ?? 'aes-256-gcm');
      // Password not returned from API for security
    }
  }, [ssConfig]);

  const handleSaveSS = () => {
    if (!ssPassword && !ssConfig?.enabled) {
      return; // Password required when enabling
    }
    updateSS.mutate({
      enabled: ssEnabled,
      port: parseInt(ssPort, 10),
      cipher: ssCipher,
      ...(ssPassword && { password: ssPassword }),
    });
  };

  if (isLoading) {
    return <div className="py-8 text-center text-muted-foreground">{t('common.loading')}</div>;
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Shield className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('shadowsocks.config.title')}</CardTitle>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">{t('shadowsocks.config.enable')}</span>
            <Switch checked={ssEnabled} onCheckedChange={setSsEnabled} />
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('shadowsocks.config.port')}</label>
            <Input
              type="number"
              value={ssPort}
              onChange={(e) => setSsPort(e.target.value)}
              placeholder="8388"
            />
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('shadowsocks.config.cipher')}</label>
            <Select value={ssCipher} onValueChange={setSsCipher}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="aes-256-gcm">AES-256-GCM</SelectItem>
                <SelectItem value="chacha20-ietf-poly1305">
                  ChaCha20-IETF-Poly1305
                </SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('shadowsocks.config.password')}</label>
            <Input
              type="password"
              value={ssPassword}
              onChange={(e) => setSsPassword(e.target.value)}
              placeholder={ssConfig?.enabled ? '••••••••' : t('shadowsocks.config.password')}
            />
            <p className="text-xs text-muted-foreground">
              {ssConfig?.enabled ? t('shadowsocks.config.passwordHint.enabled') : t('shadowsocks.config.passwordHint.disabled')}
            </p>
          </div>
        </div>
        <Button onClick={handleSaveSS} disabled={updateSS.isPending}>
          {updateSS.isPending ? t('common.saving') : t('shadowsocks.config.save')}
        </Button>
      </CardContent>
    </Card>
  );
}
