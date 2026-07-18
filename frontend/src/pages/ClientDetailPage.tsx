import { useMemo } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  ChartLegend,
  ChartLegendContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients, useQuality, useTraffic } from '@/api/hooks';
import { formatBytes } from '@/utils/format';
import { ArrowLeft, Signal, Clock, Activity, ArrowDown, ArrowUp } from 'lucide-react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid } from 'recharts';

const makeTooltipFormatter =
  (config: ChartConfig, format: (value: number) => string) =>
  (value: unknown, name: unknown) => {
    const key = String(name);
    return (
      <div className="flex w-full items-center gap-2">
        <span
          className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
          style={{ backgroundColor: config[key]?.color }}
        />
        <span className="flex-1 text-muted-foreground">
          {config[key]?.label ?? key}
        </span>
        <span className="font-mono font-medium tabular-nums text-foreground">
          {format(Number(value))}
        </span>
      </div>
    );
  };

export default function ClientDetailPage() {
  const { port } = useParams<{ port: string }>();
  const navigate = useNavigate();
  const portNum = parseInt(port || '0', 10);

  const { data: clients } = useClients();
  const { data: quality } = useQuality(portNum);
  const { data: traffic } = useTraffic(portNum, 24);

  const client = clients?.find((c) => c.port === portNum);
  const current = quality?.current;
  const history = quality?.history ?? [];
  const buckets = traffic?.buckets ?? [];

  const trafficChartConfig = useMemo<ChartConfig>(
    () => ({
      bytes_in: { label: 'Bytes In', color: 'hsl(var(--chart-1))' },
      bytes_out: { label: 'Bytes Out', color: 'hsl(var(--chart-2))' },
    }),
    []
  );
  const qualityChartConfig = useMemo<ChartConfig>(
    () => ({
      quality_score: { label: 'Quality Score', color: 'hsl(var(--chart-3))' },
    }),
    []
  );
  const rttChartConfig = useMemo<ChartConfig>(
    () => ({
      avg_rtt_ms: { label: 'Avg RTT', color: 'hsl(var(--chart-4))' },
    }),
    []
  );
  const lossChartConfig = useMemo<ChartConfig>(
    () => ({
      loss_rate: { label: 'Loss Rate', color: 'hsl(var(--chart-5))' },
    }),
    []
  );
  const throughputChartConfig = useMemo<ChartConfig>(
    () => ({
      bytes_in_per_sec: { label: 'Bytes In/s', color: 'hsl(var(--chart-1))' },
      bytes_out_per_sec: { label: 'Bytes Out/s', color: 'hsl(var(--chart-2))' },
    }),
    []
  );

  return (
    <div className="space-y-6">
      <PageHeader
        title={`Client Port ${port}`}
        description={client?.hostname}
      >
        {current && <QualityBadge score={current.quality_score} />}
        <Button variant="outline" onClick={() => navigate('/dashboard')}>
          <ArrowLeft className="mr-2 h-4 w-4" />
          Back
        </Button>
      </PageHeader>

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
        <StatCard
          title="Quality Score"
          value={current?.quality_score ?? '-'}
          icon={<Signal className="h-4 w-4" />}
        />
        <StatCard
          title="RTT"
          value={current?.last_rtt_ms != null ? `${current.last_rtt_ms.toFixed(1)}ms` : '-'}
          icon={<Clock className="h-4 w-4" />}
        />
        <StatCard
          title="Active Connections"
          value={client?.connection_count ?? 0}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="Loss Rate"
          value={current?.loss_rate != null ? `${(current.loss_rate * 100).toFixed(1)}%` : '-'}
          icon={<Activity className="h-4 w-4" />}
        />
      </div>

      {/* Traffic Stats */}
      <div className="grid gap-4 md:grid-cols-2">
        <StatCard
          title="Total Bytes In"
          value={traffic?.total_bytes_in != null ? formatBytes(traffic.total_bytes_in) : '-'}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Total Bytes Out"
          value={traffic?.total_bytes_out != null ? formatBytes(traffic.total_bytes_out) : '-'}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Traffic Chart */}
      <Card>
        <CardHeader>
          <CardTitle>Traffic (Last 24h)</CardTitle>
        </CardHeader>
        <CardContent>
          {buckets.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No traffic data</div>
          ) : (
            <ChartContainer config={trafficChartConfig} className="h-[250px] w-full sm:h-[300px]">
              <LineChart data={buckets} margin={{ left: 12, right: 12 }}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis
                  dataKey="timestamp"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  width={70}
                  tickFormatter={(v: number) => formatBytes(v)}
                />
                <ChartTooltip
                  content={
                    <ChartTooltipContent
                      labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                      formatter={makeTooltipFormatter(trafficChartConfig, formatBytes)}
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

      {/* Quality History */}
      <Card>
        <CardHeader>
          <CardTitle>Quality History</CardTitle>
        </CardHeader>
        <CardContent>
          {history.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No quality history</div>
          ) : (
            <div className="space-y-6">
              {/* Quality Score Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">Quality Score</h4>
                <ChartContainer config={qualityChartConfig} className="h-[200px] w-full">
                  <LineChart data={history} margin={{ left: 12, right: 12 }}>
                    <CartesianGrid strokeDasharray="3 3" vertical={false} />
                    <XAxis
                      dataKey="timestamp"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis
                      domain={[0, 100]}
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                    />
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                          formatter={makeTooltipFormatter(qualityChartConfig, (v) => v.toFixed(0))}
                        />
                      }
                    />
                    <Line
                      type="monotone"
                      dataKey="quality_score"
                      stroke="var(--color-quality_score)"
                      dot={false}
                      strokeWidth={2}
                    />
                  </LineChart>
                </ChartContainer>
              </div>

              {/* RTT Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">RTT (ms)</h4>
                <ChartContainer config={rttChartConfig} className="h-[200px] w-full">
                  <LineChart data={history} margin={{ left: 12, right: 12 }}>
                    <CartesianGrid strokeDasharray="3 3" vertical={false} />
                    <XAxis
                      dataKey="timestamp"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis tickLine={false} axisLine={false} tickMargin={8} />
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                          formatter={makeTooltipFormatter(rttChartConfig, (v) => `${v.toFixed(1)}ms`)}
                        />
                      }
                    />
                    <Line
                      type="monotone"
                      dataKey="avg_rtt_ms"
                      stroke="var(--color-avg_rtt_ms)"
                      dot={false}
                      strokeWidth={2}
                    />
                  </LineChart>
                </ChartContainer>
              </div>

              {/* Loss Rate Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">Loss Rate</h4>
                <ChartContainer config={lossChartConfig} className="h-[200px] w-full">
                  <LineChart data={history} margin={{ left: 12, right: 12 }}>
                    <CartesianGrid strokeDasharray="3 3" vertical={false} />
                    <XAxis
                      dataKey="timestamp"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      tickFormatter={(v: number) => `${(v * 100).toFixed(0)}%`}
                    />
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                          formatter={makeTooltipFormatter(lossChartConfig, (v) => `${(v * 100).toFixed(1)}%`)}
                        />
                      }
                    />
                    <Line
                      type="monotone"
                      dataKey="loss_rate"
                      stroke="var(--color-loss_rate)"
                      dot={false}
                      strokeWidth={2}
                    />
                  </LineChart>
                </ChartContainer>
              </div>

              {/* Throughput Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">Throughput</h4>
                <ChartContainer config={throughputChartConfig} className="h-[200px] w-full">
                  <LineChart data={history} margin={{ left: 12, right: 12 }}>
                    <CartesianGrid strokeDasharray="3 3" vertical={false} />
                    <XAxis
                      dataKey="timestamp"
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis
                      tickLine={false}
                      axisLine={false}
                      tickMargin={8}
                      width={70}
                      tickFormatter={(v: number) => formatBytes(v)}
                    />
                    <ChartTooltip
                      content={
                        <ChartTooltipContent
                          labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
                          formatter={makeTooltipFormatter(throughputChartConfig, formatBytes)}
                        />
                      }
                    />
                    <ChartLegend content={<ChartLegendContent />} />
                    <Line
                      type="monotone"
                      dataKey="bytes_in_per_sec"
                      stroke="var(--color-bytes_in_per_sec)"
                      dot={false}
                      strokeWidth={2}
                    />
                    <Line
                      type="monotone"
                      dataKey="bytes_out_per_sec"
                      stroke="var(--color-bytes_out_per_sec)"
                      dot={false}
                      strokeWidth={2}
                    />
                  </LineChart>
                </ChartContainer>
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
