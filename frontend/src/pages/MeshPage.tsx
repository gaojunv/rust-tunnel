import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQuery } from '@tanstack/react-query';
import { clientsApi } from '@/api/client';
import { cn } from '@/lib/utils';

type StatusTone = 'critical' | 'warning' | 'connected' | 'unknown';

const STATUS_TONES: Record<StatusTone, string> = {
  critical: 'bg-red-500/10 text-red-500 border-red-500/25',
  warning: 'bg-amber-500/10 text-amber-500 border-amber-500/25',
  connected: 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25',
  unknown: 'text-muted-foreground',
};

const STATUS_DOTS: Record<StatusTone, string> = {
  critical: 'bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]',
  warning: 'bg-amber-500 shadow-[0_0_6px_hsl(38_92%_55%/0.8)]',
  connected: 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]',
  unknown: 'bg-muted-foreground/50',
};

const STATUS_LABEL_KEYS = {
  critical: 'common.status.critical',
  warning: 'common.status.warning',
  connected: 'common.status.connected',
  unknown: 'common.status.unknown',
} as const;

export default function MeshPage() {
  const { t } = useTranslation();
  const { data: clients = [], isLoading } = useQuery({
    queryKey: ['clients', 'mesh'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('mesh.title')}
        description={t('mesh.description')}
      />

      <Card>
        <CardHeader>
          <CardTitle>{t('mesh.clients')}</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              {t('common.loading')}
            </div>
          ) : clients.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              {t('mesh.empty')}
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('mesh.table.name')}</TableHead>
                  <TableHead>{t('mesh.table.status')}</TableHead>
                  <TableHead>{t('mesh.table.hostname')}</TableHead>
                  <TableHead>{t('mesh.table.version')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients.map((client) => {
                  const tone: StatusTone = client.online ? 'connected' : 'unknown';
                  return (
                    <TableRow key={client.name}>
                      <TableCell className="font-medium">
                        {client.name}
                      </TableCell>
                      <TableCell>
                        <Badge
                          variant="outline"
                          className={cn('gap-1.5 font-medium', STATUS_TONES[tone])}
                        >
                          <span
                            className={cn(
                              'h-1.5 w-1.5 rounded-full',
                              STATUS_DOTS[tone]
                            )}
                          />
                          {t(STATUS_LABEL_KEYS[tone])}
                        </Badge>
                      </TableCell>
                      <TableCell>{client.hostname ?? '-'}</TableCell>
                      <TableCell className="text-muted-foreground">
                        {client.client_version ?? '-'}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
