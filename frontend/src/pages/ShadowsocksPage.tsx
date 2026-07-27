import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { MetricAreaChart } from '@/components/charts/MetricAreaChart';
import { useShadowsocksConfig, useStatsSummary, useStatsQuery } from '@/api/hooks';
import ShadowsocksConfigCard from '@/components/shadowsocks/ShadowsocksConfigCard';
import { formatBytes, formatBps } from '@/utils/format';
import { Shield, ArrowDown, ArrowUp, Signal, Users } from 'lucide-react';

export default function ShadowsocksPage() {
  const { t } = useTranslation();
  const { data: config } = useShadowsocksConfig();
  const { data: summary } = useStatsSummary();
  const stats = summary?.shadowsocks;

  const port = config?.port;
  const { startIso, endIso } = useMemo(() => {
    const end = Date.now();
    return {
      startIso: new Date(end - 6 * 60 * 60 * 1000).toISOString(),
      endIso: new Date(end).toISOString(),
    };
  }, []);
  const { data: snapshots = [] } = useStatsQuery(
    ['shadowsocks'],
    port ? [`ss:${port}`] : undefined,
    port ? startIso : undefined,
    port ? endIso : undefined,
  );

  const chartData = snapshots.map((snap) => ({
    timestamp: snap.timestamp,
    bytes_in: snap.bytes_in_rate,
    bytes_out: snap.bytes_out_rate,
  }));

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('shadowsocks.title')}
        description={t('shadowsocks.description')}
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-4">
        <StatCard
          title={t('shadowsocks.stats.status')}
          value={config?.enabled ? t('common.status.active') : t('common.status.inactive')}
          icon={<Shield className="h-4 w-4" />}
        />
        <StatCard
          title={t('shadowsocks.stats.bytesIn')}
          value={formatBytes(stats?.total_bytes_in ?? 0)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title={t('shadowsocks.stats.bytesOut')}
          value={formatBytes(stats?.total_bytes_out ?? 0)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
        <StatCard
          title={t('shadowsocks.stats.activeConnections')}
          value={stats?.total_conns ?? 0}
          icon={<Users className="h-4 w-4" />}
        />
      </div>

      {/* Server Configuration */}
      <ShadowsocksConfigCard />

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Signal className="h-5 w-5" />
            {t('shadowsocks.throughput')}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <MetricAreaChart
            data={chartData}
            series={[
              { dataKey: 'bytes_in', label: t('shadowsocks.chart.bytesIn'), colorVar: 'hsl(var(--chart-1))' },
              { dataKey: 'bytes_out', label: t('shadowsocks.chart.bytesOut'), colorVar: 'hsl(var(--chart-2))' },
            ]}
            yFormatter={formatBps}
            className="h-[250px] w-full sm:h-[300px]"
            emptyText={t('shadowsocks.chart.empty')}
          />
        </CardContent>
      </Card>
    </div>
  );
}
