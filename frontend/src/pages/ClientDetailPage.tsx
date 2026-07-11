import { useParams, useNavigate } from 'react-router-dom';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients, useQuality, useTraffic } from '@/api/hooks';
import { ArrowLeft, Signal, Clock, Activity, ArrowDown, ArrowUp } from 'lucide-react';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts';

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
}

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
            <ResponsiveContainer width="100%" height={300}>
              <LineChart data={buckets}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis tickFormatter={(v: number) => formatBytes(v)} />
                <Tooltip
                  labelFormatter={(ts: string) => new Date(ts).toLocaleString()}
                  formatter={(value: number) => formatBytes(value)}
                />
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
                <ResponsiveContainer width="100%" height={200}>
                  <LineChart data={history}>
                    <CartesianGrid strokeDasharray="3 3" />
                    <XAxis
                      dataKey="timestamp"
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis domain={[0, 100]} />
                    <Tooltip
                      labelFormatter={(ts: string) => new Date(ts).toLocaleString()}
                    />
                    <Line
                      type="monotone"
                      dataKey="quality_score"
                      stroke="#8b5cf6"
                      name="Quality Score"
                      dot={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>

              {/* RTT Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">RTT (ms)</h4>
                <ResponsiveContainer width="100%" height={200}>
                  <LineChart data={history}>
                    <CartesianGrid strokeDasharray="3 3" />
                    <XAxis
                      dataKey="timestamp"
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis />
                    <Tooltip
                      labelFormatter={(ts: string) => new Date(ts).toLocaleString()}
                      formatter={(value: number) => `${value.toFixed(1)}ms`}
                    />
                    <Line
                      type="monotone"
                      dataKey="avg_rtt_ms"
                      stroke="#f59e0b"
                      name="Avg RTT"
                      dot={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>

              {/* Loss Rate Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">Loss Rate</h4>
                <ResponsiveContainer width="100%" height={200}>
                  <LineChart data={history}>
                    <CartesianGrid strokeDasharray="3 3" />
                    <XAxis
                      dataKey="timestamp"
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis tickFormatter={(v: number) => `${(v * 100).toFixed(0)}%`} />
                    <Tooltip
                      labelFormatter={(ts: string) => new Date(ts).toLocaleString()}
                      formatter={(value: number) => `${(value * 100).toFixed(1)}%`}
                    />
                    <Line
                      type="monotone"
                      dataKey="loss_rate"
                      stroke="#ef4444"
                      name="Loss Rate"
                      dot={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>

              {/* Throughput Chart */}
              <div>
                <h4 className="text-sm font-medium mb-2">Throughput</h4>
                <ResponsiveContainer width="100%" height={200}>
                  <LineChart data={history}>
                    <CartesianGrid strokeDasharray="3 3" />
                    <XAxis
                      dataKey="timestamp"
                      tickFormatter={(ts: string) => new Date(ts).toLocaleTimeString()}
                    />
                    <YAxis tickFormatter={(v: number) => formatBytes(v)} />
                    <Tooltip
                      labelFormatter={(ts: string) => new Date(ts).toLocaleString()}
                      formatter={(value: number) => formatBytes(value)}
                    />
                    <Line
                      type="monotone"
                      dataKey="bytes_in_per_sec"
                      stroke="#3b82f6"
                      name="Bytes In/s"
                      dot={false}
                    />
                    <Line
                      type="monotone"
                      dataKey="bytes_out_per_sec"
                      stroke="#10b981"
                      name="Bytes Out/s"
                      dot={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
