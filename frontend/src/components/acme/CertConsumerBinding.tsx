import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Link, Shield, Server, Globe } from 'lucide-react';
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
                  className="flex items-center justify-between rounded-md border p-3"
                >
                  <div className="flex items-center gap-3">
                    <div
                      className={
                        active
                          ? 'text-green-500'
                          : 'text-muted-foreground'
                      }
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
                    variant={active ? 'default' : 'secondary'}
                    className={active ? 'bg-green-500/10 text-green-700 border-green-200' : ''}
                  >
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
