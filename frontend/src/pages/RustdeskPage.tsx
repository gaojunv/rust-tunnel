import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { MetricAreaChart } from '@/components/charts/MetricAreaChart';
import { StatCard } from '@/components/shared/StatCard';
import { ErrorBanner } from '@/components/shared/ErrorBanner';
import { EmptyState } from '@/components/shared/EmptyState';
import { PageHeader } from '@/components/layout/PageHeader';
import { useRustdeskConfig, useStatsSummary, useStatsQuery } from '@/api/hooks';
import RustdeskConfigCard from '@/components/rustdesk/RustdeskConfigCard';
import { formatBps } from '@/utils/format';
import { MonitorSmartphone, Signal, Users, ArrowLeftRight, Gauge, RadioTower } from 'lucide-react';

export default function RustdeskPage() {
  const { t } = useTranslation();
  const { data: config, isError: configError } = useRustdeskConfig();
  const { isError: summaryError, refetch: refetchSummary } = useStatsSummary();

  const relayPort = config?.relay_port;
  const { startIso, endIso } = useMemo(() => {
    const end = Date.now();
    return {
      startIso: new Date(end - 6 * 60 * 60 * 1000).toISOString(),
      endIso: new Date(end).toISOString(),
    };
  }, []);
  const { data: snapshots = [] } = useStatsQuery(
    ['rustdesk'],
    relayPort ? [`rustdesk:${relayPort}`] : undefined,
    relayPort ? startIso : undefined,
    relayPort ? endIso : undefined,
  );

  const chartData = snapshots.map((snap) => ({
    timestamp: snap.timestamp,
    bytes_in: snap.bytes_in_rate,
    bytes_out: snap.bytes_out_rate,
  }));

  // 吞吐量取最近一个快照的双向速率之和
  const latest = snapshots.length > 0 ? snapshots[snapshots.length - 1] : undefined;
  const throughput = latest ? latest.bytes_in_rate + latest.bytes_out_rate : 0;
  const serviceRunning = config?.running ?? config?.enabled ?? false;

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('rustdesk.title')}
        description={t('rustdesk.description')}
      />

      {/* Stats */}
      {summaryError && (
        <ErrorBanner onRetry={() => void refetchSummary()} />
      )}
      {/* Stats：共 5 张（状态/在线对端/已知对端/当前中继对/吞吐），宽屏取 5 列以免末位掉行 */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-5">
        <StatCard
          title={t('rustdesk.stats.status')}
          value={configError ? '—' : (serviceRunning ? t('common.status.active') : t('common.status.inactive'))}
          icon={<MonitorSmartphone className="h-4 w-4" />}
        />
        <StatCard
          title={t('rustdesk.stats.onlinePeers')}
          value={configError ? '—' : (config?.online_peers ?? 0)}
          icon={<Users className="h-4 w-4" />}
        />
        <StatCard
          title={t('rustdesk.stats.knownPeers')}
          value={configError ? '—' : (config?.known_peers ?? 0)}
          icon={<RadioTower className="h-4 w-4" />}
        />
        <StatCard
          title={t('rustdesk.stats.relayPairs')}
          value={configError ? '—' : (config?.relay_pairs ?? 0)}
          icon={<ArrowLeftRight className="h-4 w-4" />}
        />
        <StatCard
          title={t('rustdesk.stats.throughput')}
          value={summaryError ? '—' : formatBps(throughput)}
          icon={<Gauge className="h-4 w-4" />}
        />
      </div>

      {/* Server Configuration */}
      <RustdeskConfigCard />

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Signal className="h-5 w-5" />
            {t('rustdesk.throughput')}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {chartData.length === 0 ? (
            <EmptyState icon={Signal} title={t('rustdesk.chart.empty')} />
          ) : (
            <MetricAreaChart
              data={chartData}
              series={[
                { dataKey: 'bytes_in', label: t('rustdesk.chart.bytesIn'), colorVar: 'hsl(var(--chart-1))' },
                { dataKey: 'bytes_out', label: t('rustdesk.chart.bytesOut'), colorVar: 'hsl(var(--chart-2))' },
              ]}
              yFormatter={formatBps}
              className="h-[250px] w-full sm:h-[300px]"
              emptyText={t('rustdesk.chart.empty')}
            />
          )}
        </CardContent>
      </Card>
    </div>
  );
}
