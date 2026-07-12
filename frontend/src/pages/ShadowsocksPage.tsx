import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useShadowsocksConfig, useShadowsocksStats, useShadowsocksQuality } from '@/api/hooks';
import { Shield, ArrowDown, ArrowUp, Signal } from 'lucide-react';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts';

export default function ShadowsocksPage() {
  const { data: config } = useShadowsocksConfig();
  const { data: stats } = useShadowsocksStats();
  const { data: qualityData } = useShadowsocksQuality();

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  const qualityHistory = qualityData?.[0]?.history ?? [];
  const chartData = qualityHistory.map((sample) => ({
    timestamp: sample.timestamp,
    rtt: sample.avg_rtt_ms,
    loss: sample.loss_rate * 100,
    score: sample.quality_score,
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

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Signal className="h-5 w-5" />
            Throughput
          </CardTitle>
        </CardHeader>
        <CardContent>
          {chartData.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              No throughput data available
            </div>
          ) : (
            <ResponsiveContainer width="100%" height={300}>
              <LineChart data={chartData}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts) =>
                    new Date(ts).toLocaleTimeString([], {
                      hour: '2-digit',
                      minute: '2-digit',
                    })
                  }
                />
                <YAxis tickFormatter={(v) => formatBytes(v) + '/s'} />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                  formatter={(value: number) => formatBytes(value) + '/s'}
                />
                <Legend />
                <Line
                  type="monotone"
                  dataKey="bytes_in"
                  stroke="#3b82f6"
                  name="Bytes In"
                  dot={false}
                />
                <Line
                  type="monotone"
                  dataKey="bytes_out"
                  stroke="#10b981"
                  name="Bytes Out"
                  dot={false}
                />
              </LineChart>
            </ResponsiveContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
