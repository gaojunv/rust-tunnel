import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { MetricAreaChart } from '@/components/charts/MetricAreaChart';
import { QualityHistoryCharts } from '@/components/charts/QualityHistoryCharts';
import { useShadowsocksConfig, useShadowsocksStats, useShadowsocksQuality } from '@/api/hooks';
import ShadowsocksConfigCard from '@/components/shadowsocks/ShadowsocksConfigCard';
import { formatBytes, formatBps } from '@/utils/format';
import { Shield, ArrowDown, ArrowUp, Signal } from 'lucide-react';

export default function ShadowsocksPage() {
  const { data: config } = useShadowsocksConfig();
  const { data: stats } = useShadowsocksStats();
  const { data: qualityData } = useShadowsocksQuality();

  const qualityHistory = qualityData?.[0]?.history ?? [];
  const chartData = qualityHistory.map((sample) => ({
    timestamp: sample.timestamp,
    bytes_in: sample.bytes_in_per_sec,
    bytes_out: sample.bytes_out_per_sec,
  }));

  return (
    <div className="space-y-6">
      <PageHeader
        title="Shadowsocks"
        description="Monitor the Shadowsocks proxy server"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
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
      </div>

      {/* Server Configuration */}
      <ShadowsocksConfigCard />

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

      {/* Quality History */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Signal className="h-5 w-5" />
            Connection Quality
          </CardTitle>
        </CardHeader>
        <CardContent>
          <QualityHistoryCharts history={qualityHistory} />
        </CardContent>
      </Card>
    </div>
  );
}
