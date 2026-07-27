import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useRequestAcmeCertificate, useDnsProviders } from '@/api/hooks';
import type { ChallengeType } from '@/types';

interface AcmeRequestDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AcmeRequestDialog({ open, onOpenChange }: AcmeRequestDialogProps) {
  const { t } = useTranslation();
  const [domain, setDomain] = useState('');
  const [challengeType, setChallengeType] = useState<ChallengeType>('http-01');
  const requestMutation = useRequestAcmeCertificate();
  const { data: dnsData } = useDnsProviders();
  const hasDnsProvider = !!dnsData?.config;

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    requestMutation.mutate(
      { domain, challengeType },
      {
        onSuccess: () => {
          onOpenChange(false);
          setDomain('');
          setChallengeType('http-01');
        },
      }
    );
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('acme.request.title')}</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('acme.request.domain')}</label>
            <Input
              value={domain}
              onChange={(e) => setDomain(e.target.value)}
              placeholder="example.com"
              required
            />
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">{t('acme.request.challengeType')}</label>
            <Select
              value={challengeType}
              onValueChange={(value) =>
                setChallengeType(value as ChallengeType)
              }
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="http-01">
                  {t('acme.request.http01')}
                  <Badge variant="outline" className="ml-2 text-xs text-primary border-primary/25">
                    {t('acme.request.recommended')}
                  </Badge>
                </SelectItem>
                <SelectItem value="dns-01" disabled={!hasDnsProvider}>
                  {t('acme.request.dns01')}
                  {!hasDnsProvider && (
                    <Badge variant="secondary" className="ml-2 text-xs">
                      {t('acme.request.noDnsProvider')}
                    </Badge>
                  )}
                </SelectItem>
              </SelectContent>
            </Select>
            {challengeType === 'http-01' ? (
              <p className="text-xs text-muted-foreground">
                {t('acme.request.http01Desc')}
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                {t('acme.request.dns01Desc')}
              </p>
            )}
          </div>

          {requestMutation.isError && (
            <p className="text-sm text-destructive">
              {t('acme.request.error')}
            </p>
          )}
          <Button
            type="submit"
            disabled={requestMutation.isPending}
            className="w-full"
          >
            {requestMutation.isPending ? t('acme.request.requesting') : t('acme.request.submit')}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
