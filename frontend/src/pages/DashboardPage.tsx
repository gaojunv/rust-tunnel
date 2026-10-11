import { useNavigate } from 'react-router-dom';
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
import { Skeleton } from '@/components/ui/skeleton';
import { StatCard } from '@/components/shared/StatCard';
import { TrafficAreaChart } from '@/components/charts/TrafficAreaChart';
import { PageHeader } from '@/components/layout/PageHeader';
import { EmptyState } from '@/components/shared/EmptyState';
import { ErrorBanner } from '@/components/shared/ErrorBanner';
import { ErrorState } from '@/components/shared/ErrorState';
import { useStatsSummary, useStatsStream, useClients } from '@/api/hooks';
import { formatBytes } from '@/utils/format';
import { cn } from '@/lib/utils';
import {
  Users,
  Activity,
  ArrowDown,
  ArrowUp,
  ExternalLink,
  Network,
  RefreshCw,
  Shield,
  Globe,
  MonitorSmartphone,
} from 'lucide-react';

function StatsOverview() {
  const { data: summary, isLoading, isError, refetch } = useStatsSummary();
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
    {
      title: t('dashboard.statsOverview.rustdesk'),
      icon: <MonitorSmartphone className="h-4 w-4" />,
      entity: summary?.rustdesk,
    },
  ];

  return (
    <div className="space-y-4">
      {isError && <ErrorBanner onRetry={() => void refetch()} />}
      {/* 概览卡共 5 张（客户端/反代/SS/Trojan/RustDesk），宽屏取 5 列以免末位掉行 */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 md:gap-6 lg:grid-cols-5">
        {cards.map((card) => (
          <StatCard
            key={card.title}
            title={card.title}
            value={
              isLoading || isError
                ? '—'
                : formatBytes(
                    (card.entity?.total_bytes_in ?? 0) +
                      (card.entity?.total_bytes_out ?? 0)
                  )
            }
            description={
              isLoading || isError
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
    </div>
  );
}

export default function DashboardPage() {
  const navigate = useNavigate();
  const { t } = useTranslation();
  const { data: clients = [], isLoading: clientsLoading, isError: clientsError, isFetching: clientsFetching, refetch: refetchClients } = useClients();
  const { data: summary, isFetching: statsFetching, refetch: refetchStats } = useStatsSummary();

  const refreshing = clientsFetching || statsFetching;
  const refreshAll = () => {
    void refetchClients();
    void refetchStats();
  };

  const connectedClients = clients.filter((c) => c.online).length;
  const entities = summary
    ? [summary.clients, summary.proxy, summary.shadowsocks, summary.trojan, summary.rustdesk]
    : [];
  const activeConnections = entities.reduce((sum, e) => sum + e.total_conns, 0);
  const totalBytesIn = entities.reduce((sum, e) => sum + e.total_bytes_in, 0);
  const totalBytesOut = entities.reduce((sum, e) => sum + e.total_bytes_out, 0);

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('dashboard.title')}
        description={t('dashboard.description')}
      >
        <Button variant="outline" onClick={refreshAll} disabled={refreshing}>
          <RefreshCw className={cn('mr-2 h-4 w-4', refreshing && 'animate-spin')} />
          {t('common.refresh')}
        </Button>
      </PageHeader>

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
            <div className="space-y-3 py-2">
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-3/4" />
            </div>
          ) : clientsError ? (
            <ErrorState onRetry={() => void refetchClients()} />
          ) : clients.length === 0 ? (
            <EmptyState icon={Users} title={t('dashboard.clientList.empty')} />
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
