import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { MetricAreaChart } from '@/components/charts/MetricAreaChart';
import { QualityHistoryCharts } from '@/components/charts/QualityHistoryCharts';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useTrojanConfig, useTrojanStats, useTrojanQuality } from '@/api/hooks';
import TrojanConfigCard from '@/components/trojan/TrojanConfigCard';
import { formatBytes, formatBps } from '@/utils/format';
import {
  Shield,
  ArrowDown,
  ArrowUp,
  Signal,
  Users,
  Activity,
} from 'lucide-react';

export default function TrojanPage() {
  const { data: config } = useTrojanConfig();
  const { data: stats } = useTrojanStats();
  const { data: qualityData } = useTrojanQuality();

  const qualityHistory = qualityData?.[0]?.history ?? [];
  const chartData = qualityHistory.map((sample) => ({
    timestamp: sample.timestamp,
    bytes_in: sample.bytes_in_per_sec,
    bytes_out: sample.bytes_out_per_sec,
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
          value={stats?.active_connections ?? 0}
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

      {/* Quality History Chart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Activity className="h-5 w-5" />
            Quality History
          </CardTitle>
        </CardHeader>
        <CardContent>
          <QualityHistoryCharts history={qualityHistory} />
        </CardContent>
      </Card>
    </div>
  );
}
