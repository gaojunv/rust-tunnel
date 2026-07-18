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
  label: string;
  description: string;
  icon: React.ReactNode;
}

const CONSUMERS: ConsumerItem[] = [
  {
    key: 'api_tls',
    label: 'API Server TLS',
    description: 'HTTPS for the management API',
    icon: <Server className="h-4 w-4" />,
  },
  {
    key: 'trojan',
    label: 'Trojan Proxy',
    description: 'TLS for Trojan proxy connections',
    icon: <Shield className="h-4 w-4" />,
  },
  {
    key: 'control_tls',
    label: 'Control Channel TLS',
    description: 'TLS for client control connections',
    icon: <Link className="h-4 w-4" />,
  },
  {
    key: 'reverse_proxy',
    label: 'Reverse Proxy',
    description: 'TLS for reverse proxy rules',
    icon: <Globe className="h-4 w-4" />,
  },
];

export function CertConsumerBinding({ consumers }: CertConsumerBindingProps) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Certificate Consumers</CardTitle>
      </CardHeader>
      <CardContent>
        {!consumers ? (
          <p className="text-sm text-muted-foreground">
            No consumer binding information available.
          </p>
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
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
                      <div className="text-sm font-medium">{item.label}</div>
                      <div className="text-xs text-muted-foreground">
                        {item.description}
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
                    {active ? 'Active' : 'Inactive'}
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
