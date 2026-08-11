import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Link, Shield, Server, Globe } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { CertConsumers } from '@/types';

interface CertConsumerBindingProps {
  consumers?: CertConsumers;
}

interface ConsumerItem {
  key: keyof CertConsumers;
  icon: React.ReactNode;
}

const CONSUMERS: ConsumerItem[] = [
  {
    key: 'api_tls',
    icon: <Server className="h-4 w-4" />,
  },
  {
    key: 'trojan',
    icon: <Shield className="h-4 w-4" />,
  },
  {
    key: 'control_tls',
    icon: <Link className="h-4 w-4" />,
  },
  {
    key: 'reverse_proxy',
    icon: <Globe className="h-4 w-4" />,
  },
];

export function CertConsumerBinding({ consumers }: CertConsumerBindingProps) {
  const { t } = useTranslation();
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('acme.consumers.title')}</CardTitle>
      </CardHeader>
      <CardContent>
        {!consumers ? (
          <p className="text-sm text-muted-foreground">
            {t('acme.consumers.empty')}
          </p>
        ) : (
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            {CONSUMERS.map((item) => {
              const active = consumers[item.key];
              return (
                <div
                  key={item.key}
                  className={cn(
                    'flex items-center justify-between rounded-lg border p-3 transition-colors',
                    active ? 'border-emerald-500/25 bg-emerald-500/5' : 'bg-muted/30'
                  )}
                >
                  <div className="flex items-center gap-3">
                    <div
                      className={cn(
                        'flex h-8 w-8 items-center justify-center rounded-md border',
                        active
                          ? 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25'
                          : 'bg-muted text-muted-foreground'
                      )}
                    >
                      {item.icon}
                    </div>
                    <div>
                      <div className="text-sm font-medium">{t(`acme.consumers.items.${item.key}.label` as const)}</div>
                      <div className="text-xs text-muted-foreground">
                        {t(`acme.consumers.items.${item.key}.desc` as const)}
                      </div>
                    </div>
                  </div>
                  <Badge
                    variant="outline"
                    className={cn(
                      'gap-1.5 font-medium',
                      active
                        ? 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25'
                        : 'text-muted-foreground'
                    )}
                  >
                    <span
                      className={cn(
                        'h-1.5 w-1.5 rounded-full',
                        active
                          ? 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]'
                          : 'bg-muted-foreground/50'
                      )}
                    />
                    {active ? t('common.status.active') : t('common.status.inactive')}
                  </Badge>
                </div>
              );
            })}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
