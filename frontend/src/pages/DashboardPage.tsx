import { useNavigate } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Button } from '@/components/ui/button';
import { StatCard } from '@/components/shared/StatCard';
import { TrafficAreaChart } from '@/components/charts/TrafficAreaChart';
import { PageHeader } from '@/components/layout/PageHeader';
import { clientsApi } from '@/api/client';
import { useStatsSummary, useStatsStream } from '@/api/hooks';
import { formatBytes } from '@/utils/format';
import {
  Users,
  Activity,
  ArrowDown,
  ArrowUp,
  ExternalLink,
  Network,
  Shield,
  Globe,
} from 'lucide-react';

function StatsOverview() {
  const { data: summary, isLoading } = useStatsSummary();
  const { t } = useTranslation();
  useStatsStream();

  const cards = [
    {
      title: t('dashboard.statsOverview.clients'),
      icon: <Users className="h-4 w-4" />,
      entity: summary?.clients,
    },
    {
      title: t('dashboard.statsOverview.reverseProxy'),
      icon: <Network className="h-4 w-4" />,
      entity: summary?.proxy,
    },
    {
      title: t('dashboard.statsOverview.shadowsocks'),
      icon: <Shield className="h-4 w-4" />,
      entity: summary?.shadowsocks,
    },
    {
      title: t('dashboard.statsOverview.trojan'),
      icon: <Globe className="h-4 w-4" />,
      entity: summary?.trojan,
    },
  ];

  return (
    <div className="grid grid-cols-1 gap-4 md:grid-cols-2 md:gap-6 lg:grid-cols-4">
      {cards.map((card) => (
        <StatCard
          key={card.title}
          title={card.title}
          value={
            isLoading
              ? '—'
              : formatBytes(
                  (card.entity?.total_bytes_in ?? 0) +
                    (card.entity?.total_bytes_out ?? 0)
                )
          }
          description={
            isLoading
              ? undefined
              : t('dashboard.statsOverview.connections', {
                  count: card.entity?.total_conns ?? 0,
                  entities: card.entity?.entity_count ?? 0,
                })
          }
          icon={card.icon}
        />
      ))}
    </div>
  );
}

export default function DashboardPage() {
  const navigate = useNavigate();
  const { t } = useTranslation();
  const { data: clients = [], isLoading: clientsLoading, isError: clientsError, refetch: refetchClients } = useQuery({
    queryKey: ['clients', 'dashboard'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });
  const { data: summary } = useStatsSummary();

  const connectedClients = clients.filter((c) => c.online).length;
  const entities = summary
    ? [summary.clients, summary.proxy, summary.shadowsocks, summary.trojan]
    : [];
  const activeConnections = entities.reduce((sum, e) => sum + e.total_conns, 0);
  const totalBytesIn = entities.reduce((sum, e) => sum + e.total_bytes_in, 0);
  const totalBytesOut = entities.reduce((sum, e) => sum + e.total_bytes_out, 0);

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('dashboard.title')}
        description={t('dashboard.description')}
      />

      {/* Unified Stats Overview */}
      <StatsOverview />

      {/* Stats Grid */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 md:gap-6 lg:grid-cols-4">
        <StatCard
          title={t('dashboard.connectedClients')}
          value={connectedClients}
          icon={<Users className="h-4 w-4" />}
        />
        <StatCard
          title={t('dashboard.activeConnections')}
          value={activeConnections}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title={t('dashboard.totalBytesIn')}
          value={formatBytes(totalBytesIn)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title={t('dashboard.totalBytesOut')}
          value={formatBytes(totalBytesOut)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Traffic Chart */}
      <TrafficAreaChart />

      {/* Client List */}
      <Card>
        <CardHeader>
          <CardTitle>{t('dashboard.clientList.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          {clientsLoading ? (
            <div className="py-12 text-center text-sm text-muted-foreground">
              {t('common.loading')}
            </div>
          ) : clientsError ? (
            <div className="flex flex-col items-center justify-center gap-3 py-12 text-center">
              <p className="text-sm text-destructive">{t('common.loadFailed')}</p>
              <Button variant="outline" size="sm" onClick={() => void refetchClients()}>
                {t('common.retry')}
              </Button>
            </div>
          ) : clients.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 py-12 text-muted-foreground">
              <Users className="h-8 w-8 opacity-40" />
              <p className="text-sm">{t('dashboard.clientList.empty')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.name')}
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.hostname')}
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.status')}
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.version')}
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.referenced')}
                  </TableHead>
                  <TableHead className="text-right text-xs uppercase tracking-wider">
                    {t('dashboard.clientList.actions')}
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients.map((client) => (
                  <TableRow key={client.name}>
                    <TableCell className="font-medium">{client.name}</TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.hostname ?? '-'}
                    </TableCell>
                    <TableCell>
                      <span
                        className={`inline-flex items-center rounded-full px-2 py-1 text-xs font-medium ${
                          client.online
                            ? 'bg-emerald-500/10 text-emerald-500'
                            : 'bg-muted text-muted-foreground'
                        }`}
                      >
                        {client.online ? t('common.status.online') : t('common.status.offline')}
                      </span>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.client_version ?? '-'}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.referenced_by_rules}
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={t('dashboard.clientList.viewDetails', { name: client.name })}
                        className="h-8 w-8 text-muted-foreground hover:text-foreground"
                        onClick={() => navigate(`/clients/${client.name}`)}
                      >
                        <ExternalLink className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
