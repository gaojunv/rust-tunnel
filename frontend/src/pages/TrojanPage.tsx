import { useMemo } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  ChartLegend,
  ChartLegendContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useTrojanConfig, useTrojanStats, useTrojanQuality } from '@/api/hooks';
import { formatBytes, formatBps } from '@/utils/format';
import {
  Shield,
  ArrowDown,
  ArrowUp,
  Signal,
  Users,
  Activity,
} from 'lucide-react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid } from 'recharts';

export default function TrojanPage() {
  const { data: config } = useTrojanConfig();
  const { data: stats } = useTrojanStats();
  const { data: qualityData } = useTrojanQuality();

  const qualityHistory = qualityData?.[0]?.history ?? [];
  const chartData = qualityHistory.map((sample) => ({
    timestamp: sample.timestamp,
    rtt: sample.avg_rtt_ms,
    loss: sample.loss_rate * 100,
    score: sample.quality_score,
    bytes_in: sample.bytes_in_per_sec,
    bytes_out: sample.bytes_out_per_sec,
  }));

  const throughputChartConfig = useMemo<ChartConfig>(
    () => ({
      bytes_in: { label: 'Bytes In', color: 'hsl(var(--chart-1))' },
      bytes_out: { label: 'Bytes Out', color: 'hsl(var(--chart-2))' },
    }),
    []
  );
  const qualityChartConfig = useMemo<ChartConfig>(
    () => ({
      rtt: { label: 'RTT (ms)', color: 'hsl(var(--chart-4))' },
      loss: { label: 'Loss (%)', color: 'hsl(var(--chart-5))' },
      score: { label: 'Quality Score', color: 'hsl(var(--chart-3))' },
    }),
    []
  );

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
            <ChartContainer config={throughputChartConfig} className="h-[250px] w-full sm:h-[300px]">
              <LineChart data={chartData} margin={{ left: 12, right: 12 }}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis
                  dataKey="timestamp"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  tickFormatter={(ts: string) =>
                    new Date(ts).toLocaleTimeString([], {
                      hour: '2-digit',
                      minute: '2-digit',
                    })
                  }
                />
                <YAxis
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  width={80}
                  tickFormatter={(v: number) => formatBps(v)}
                />
                <ChartTooltip
                  content={
                    <ChartTooltipContent
                      labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                      formatter={(value, name) => {
                        const key = String(name);
                        return (
                          <div className="flex w-full items-center gap-2">
                            <span
                              className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
                              style={{ backgroundColor: throughputChartConfig[key]?.color }}
                            />
                            <span className="flex-1 text-muted-foreground">
                              {throughputChartConfig[key]?.label ?? key}
                            </span>
                            <span className="font-mono font-medium tabular-nums text-foreground">
                              {formatBps(Number(value))}
                            </span>
                          </div>
                        );
                      }}
                    />
                  }
                />
                <ChartLegend content={<ChartLegendContent />} />
                <Line
                  type="monotone"
                  dataKey="bytes_in"
                  stroke="var(--color-bytes_in)"
                  dot={false}
                  strokeWidth={2}
                />
                <Line
                  type="monotone"
                  dataKey="bytes_out"
                  stroke="var(--color-bytes_out)"
                  dot={false}
                  strokeWidth={2}
                />
              </LineChart>
            </ChartContainer>
          )}
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
          {chartData.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              No quality data available
            </div>
          ) : (
            <ChartContainer config={qualityChartConfig} className="h-[250px] w-full sm:h-[300px]">
              <LineChart data={chartData} margin={{ left: 12, right: 12 }}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis
                  dataKey="timestamp"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  tickFormatter={(ts: string) =>
                    new Date(ts).toLocaleTimeString([], {
                      hour: '2-digit',
                      minute: '2-digit',
                    })
                  }
                />
                <YAxis
                  yAxisId="left"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                />
                <YAxis
                  yAxisId="right"
                  orientation="right"
                  domain={[0, 100]}
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                />
                <ChartTooltip
                  content={
                    <ChartTooltipContent
                      labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                    />
                  }
                />
                <ChartLegend content={<ChartLegendContent />} />
                <Line
                  yAxisId="left"
                  type="monotone"
                  dataKey="rtt"
                  stroke="var(--color-rtt)"
                  dot={false}
                  strokeWidth={2}
                />
                <Line
                  yAxisId="left"
                  type="monotone"
                  dataKey="loss"
                  stroke="var(--color-loss)"
                  dot={false}
                  strokeWidth={2}
                />
                <Line
                  yAxisId="right"
                  type="monotone"
                  dataKey="score"
                  stroke="var(--color-score)"
                  dot={false}
                  strokeWidth={2}
                />
              </LineChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
