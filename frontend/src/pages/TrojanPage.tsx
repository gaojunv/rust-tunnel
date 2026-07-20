import { useMemo } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { MetricAreaChart } from '@/components/charts/MetricAreaChart';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useTrojanConfig, useStatsSummary, useStatsQuery } from '@/api/hooks';
import TrojanConfigCard from '@/components/trojan/TrojanConfigCard';
import { formatBytes, formatBps } from '@/utils/format';
import {
  Shield,
  ArrowDown,
  ArrowUp,
  Signal,
  Users,
} from 'lucide-react';

export default function TrojanPage() {
  const { data: config } = useTrojanConfig();
  const { data: summary } = useStatsSummary();
  const stats = summary?.trojan;

  const port = config?.port;
  const { startIso, endIso } = useMemo(() => {
    const end = Date.now();
    return {
      startIso: new Date(end - 6 * 60 * 60 * 1000).toISOString(),
      endIso: new Date(end).toISOString(),
    };
  }, []);
  const { data: snapshots = [] } = useStatsQuery(
    ['trojan'],
    port ? [`trojan:${port}`] : undefined,
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
        title="Trojan"
        description="Monitor the Trojan proxy server"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-4">
        <StatCard
          title="Status"
          value={config?.enabled ? 'Active' : 'Inactive'}
          icon={<Shield className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes In"
          value={formatBytes(stats?.total_bytes_in ?? 0)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes Out"
          value={formatBytes(stats?.total_bytes_out ?? 0)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
        <StatCard
          title="Active Connections"
          value={stats?.total_conns ?? 0}
          icon={<Users className="h-4 w-4" />}
        />
      </div>

      {/* Server Configuration */}
      <TrojanConfigCard />

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Signal className="h-5 w-5" />
            Throughput
          </CardTitle>
        </CardHeader>
        <CardContent>
          <MetricAreaChart
            data={chartData}
            series={[
              { dataKey: 'bytes_in', label: 'Bytes In', colorVar: 'hsl(var(--chart-1))' },
              { dataKey: 'bytes_out', label: 'Bytes Out', colorVar: 'hsl(var(--chart-2))' },
            ]}
            yFormatter={formatBps}
            className="h-[250px] w-full sm:h-[300px]"
            emptyText="No throughput data available"
          />
        </CardContent>
      </Card>
    </div>
  );
}
