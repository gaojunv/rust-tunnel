import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import {
  useTrojanConfig,
  useUpdateTrojanConfig,
  useTrojanStats,
  useTrojanQuality,
} from '@/api/hooks';
import {
  Shield,
  ArrowDown,
  ArrowUp,
  Signal,
  Users,
  Activity,
} from 'lucide-react';
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

export default function TrojanPage() {
  const { data: config, isLoading: configLoading } = useTrojanConfig();
  const { data: stats } = useTrojanStats();
  const { data: qualityData } = useTrojanQuality();
  const updateConfig = useUpdateTrojanConfig();

  const [enabled, setEnabled] = useState(false);
  const [port, setPort] = useState('');
  const [fallback, setFallback] = useState('');

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled ?? false);
      setPort(config.port?.toString() ?? '');
      setFallback(config.fallback ?? '');
    }
  }, [config]);

  const handleSave = () => {
    updateConfig.mutate({
      enabled,
      port: parseInt(port, 10),
      fallback: fallback || undefined,
    });
  };

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  // Build chart data from quality history
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
        title="Trojan"
        description="Configure and monitor the Trojan proxy server"
      />

      {/* Configuration Card */}
      <Card>
        <CardHeader>
          <CardTitle>Configuration</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {configLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : (
            <>
              <div className="flex items-center justify-between">
                <div>
                  <div className="font-medium">Enable Trojan</div>
                  <div className="text-sm text-muted-foreground">
                    Start the Trojan proxy server
                  </div>
                </div>
                <Switch checked={enabled} onCheckedChange={setEnabled} />
              </div>

              <div className="space-y-2">
                <label className="text-sm font-medium">Port</label>
                <Input
                  type="number"
                  value={port}
                  onChange={(e) => setPort(e.target.value)}
                  placeholder="443"
                />
              </div>

              <div className="space-y-2">
                <label className="text-sm font-medium">Fallback</label>
                <Input
                  value={fallback}
                  onChange={(e) => setFallback(e.target.value)}
                  placeholder="127.0.0.1:80"
                />
                <p className="text-xs text-muted-foreground">
                  Address to redirect traffic to when authentication fails
                </p>
              </div>

              <Button onClick={handleSave} disabled={updateConfig.isPending}>
                {updateConfig.isPending ? 'Saving...' : 'Save Configuration'}
              </Button>
            </>
          )}
        </CardContent>
      </Card>

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-4">
        <StatCard
          title="Status"
          value={enabled ? 'Active' : 'Inactive'}
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
                <YAxis yAxisId="left" />
                <YAxis
                  yAxisId="right"
                  orientation="right"
                  domain={[0, 100]}
                />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                />
                <Legend />
                <Line
                  yAxisId="left"
                  type="monotone"
                  dataKey="rtt"
                  stroke="#f59e0b"
                  name="RTT (ms)"
                  dot={false}
                />
                <Line
                  yAxisId="left"
                  type="monotone"
                  dataKey="loss"
                  stroke="#ef4444"
                  name="Loss (%)"
                  dot={false}
                />
                <Line
                  yAxisId="right"
                  type="monotone"
                  dataKey="score"
                  stroke="#8b5cf6"
                  name="Quality Score"
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
