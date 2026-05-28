import { useQuery } from 'react-query';
import { getShadowsocksConfig, getShadowsocksStats, getShadowsocksQuality } from '../api/client';
import type { ShadowsocksQuality } from '../types';
import { getQualityColor, getQualityText } from './ClientList';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

const formatBps = (bytesPerSec: number): string => formatBytes(bytesPerSec) + '/s';

const ThroughputHistory = ({ qualityList }: { qualityList: ShadowsocksQuality[] }) => {
  // Merge samples by timestamp (milliseconds) to avoid the old string-key dedup bug
  const timeMap = new Map<number, Record<string, number | string>>();
  for (const q of qualityList) {
    for (const s of q.history) {
      const ts = new Date(s.timestamp).getTime();
      if (!timeMap.has(ts)) timeMap.set(ts, { time: ts });
      const pt = timeMap.get(ts)!;
      pt[`In (Port ${q.port}) B/s`] = s.bytes_in_per_sec;
      pt[`Out (Port ${q.port}) B/s`] = s.bytes_out_per_sec;
    }
  }
  const chartData = Array.from(timeMap.values())
    .sort((a, b) => (a.time as number) - (b.time as number));

  if (chartData.length === 0) {
    return <p className="text-gray-500 text-center py-4 text-sm">No throughput data available yet</p>;
  }
  return (
    <ResponsiveContainer width="100%" height={200}>
      <LineChart data={chartData}>
        <CartesianGrid strokeDasharray="3 3" />
        <XAxis dataKey="time" tick={{ fontSize: 10 }}
          tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()} />
        <YAxis tick={{ fontSize: 10 }} tickFormatter={formatBps} />
        <Tooltip formatter={(value: number) => formatBps(value)}
          labelFormatter={(ts: number) => new Date(ts).toLocaleString()} />
        {qualityList.map(q => (
          <>
            <Line key={`in-${q.port}`} type="monotone" dataKey={`In (Port ${q.port}) B/s`}
              stroke="#3b82f6" dot={false} strokeWidth={2} />
            <Line key={`out-${q.port}`} type="monotone" dataKey={`Out (Port ${q.port}) B/s`}
              stroke="#10b981" dot={false} strokeWidth={2} />
          </>
        ))}
      </LineChart>
    </ResponsiveContainer>
  );
};

export const ShadowsocksPage = () => {
  const { data: config, isLoading: configLoading } = useQuery(
    'shadowsocks-config',
    getShadowsocksConfig,
    { refetchInterval: 5000 }
  );

  const { data: stats, isLoading: statsLoading } = useQuery(
    'shadowsocks-stats',
    getShadowsocksStats,
    { refetchInterval: 5000 }
  );

  const { data: qualityList = [] } = useQuery<ShadowsocksQuality[]>(
    'shadowsocks-quality',
    getShadowsocksQuality,
    { refetchInterval: 5000 }
  );

  if (configLoading || statsLoading) {
    return (
      <div className="flex justify-center items py-8">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {/* Configuration Card */}
      <div className="bg-white shadow rounded-lg p-6">
        <h2 className="text-lg font-semibold text-gray-900 mb-4">Shadowsocks Configuration</h2>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          <div>
            <label className="block text-sm font-medium text-gray-500">Status</label>
            <div className="flex items-center mt-1">
              <span className={`w-3 h-3 rounded-full mr-2 ${config?.enabled ? 'bg-green-500' : 'bg-gray-300'}`}></span>
              <span className="text-lg font-semibold text-gray-900">
                {config?.enabled ? 'Enabled' : 'Disabled'}
              </span>
            </div>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-500">Port</label>
            <p className="text-lg font-semibold text-gray-900">{config?.port || 'N/A'}</p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-500">Cipher</label>
            <p className="text-lg font-semibold text-gray-900">{config?.cipher || 'N/A'}</p>
          </div>
        </div>
      </div>

      {/* Statistics Card */}
      <div className="bg-white shadow rounded-lg p-6">
        <h2 className="text-lg font-semibold text-gray-900 mb-4">Traffic Statistics</h2>
        <div className="grid grid-cols-1 md:grid-cols-4 gap-4">
          <div className="bg-purple-50 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500">Enabled</label>
            <p className="text-lg font-semibold text-gray-900">
              {stats?.enabled ? 'Yes' : 'No'}
            </p>
          </div>
          <div className="bg-blue-50 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500">Port</label>
            <p className="text-lg font-semibold text-gray-900">{stats?.port || 'N/A'}</p>
          </div>
          <div className="bg-green-50 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500">Total Bytes In</label>
            <p className="text-lg font-semibold text-gray-900">
              {formatBytes(stats?.total_bytes_in || 0)}
            </p>
          </div>
          <div className="bg-orange-50 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500">Total Bytes Out</label>
            <p className="text-lg font-semibold text-gray-900">
              {formatBytes(stats?.total_bytes_out || 0)}
            </p>
          </div>
        </div>
        {stats && stats.active_connections !== undefined && (
          <div className="mt-4">
            <div className="bg-yellow-50 rounded-lg p-4">
              <label className="block text-sm font-medium text-gray-500">Active Connections</label>
              <p className="text-lg font-semibold text-gray-900">{stats.active_connections}</p>
            </div>
          </div>
        )}
      </div>

      {/* Quality History */}
      {qualityList.length > 0 && (
        <div className="bg-white shadow rounded-lg p-6">
          <h2 className="text-lg font-semibold text-gray-900 mb-4">Quality & Throughput</h2>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4 mb-4">
            {qualityList.map(q => {
              const color = getQualityColor(q.quality.quality_score);
              return (
                <div key={q.port} className="bg-gray-50 rounded-lg p-4">
                  <div className="flex items-center justify-between mb-2">
                    <span className="text-sm font-medium text-gray-700">Port {q.port}</span>
                    <span className="font-semibold" style={{ color }}>
                      Score: {q.quality.quality_score} ({getQualityText(q.quality.quality_score)})
                    </span>
                  </div>
                  <div className="grid grid-cols-2 gap-2 text-xs text-gray-600">
                    <span>In: {formatBps(q.quality.bytes_in_per_sec)}</span>
                    <span>Out: {formatBps(q.quality.bytes_out_per_sec)}</span>
                  </div>
                </div>
              );
            })}
          </div>
          <div className="bg-gray-50 p-4 rounded-lg">
            <h4 className="text-sm font-medium text-gray-700 mb-2">Throughput History (Last 60 min)</h4>
            <ThroughputHistory qualityList={qualityList} />
          </div>
        </div>
      )}
    </div>
  );
};
