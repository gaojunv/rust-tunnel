import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import type { PortTraffic } from '../types';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  // Merge data from all ports by timestamp, then sort chronologically.
  // Each unique timestamp becomes one data point that carries every port's in/out series.
  const timeMap = new Map<number, Record<string, number | string>>();

  for (const portTraffic of traffic) {
    for (const bucket of portTraffic.buckets) {
      const ts = new Date(bucket.timestamp).getTime();
      if (!timeMap.has(ts)) {
        timeMap.set(ts, { time: ts });
      }
      const point = timeMap.get(ts)!;
      point[`In (Port ${portTraffic.port})`] = bucket.bytes_in;
      point[`Out (Port ${portTraffic.port})`] = bucket.bytes_out;
    }
  }

  const chartData = Array.from(timeMap.values())
    .sort((a, b) => (a.time as number) - (b.time as number));

  return (
    <div className="bg-white p-6 rounded-lg shadow">
      <h3 className="text-lg font-medium text-gray-900 mb-4">Network Traffic</h3>
      {chartData.length > 0 ? (
        <ResponsiveContainer width="100%" height={300}>
          <LineChart data={chartData}>
            <CartesianGrid strokeDasharray="3 3" />
            <XAxis
              dataKey="time"
              tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()}
            />
            <YAxis tickFormatter={formatBytes} />
            <Tooltip
              formatter={(value: number) => formatBytes(value)}
              labelFormatter={(ts: number) => new Date(ts).toLocaleString()}
            />
            <Legend />
            {traffic.map(portTraffic => (
              <>
                <Line
                  key={`in-${portTraffic.port}`}
                  type="monotone"
                  dataKey={`In (Port ${portTraffic.port})`}
                  stroke="#3b82f6"
                  dot={false}
                />
                <Line
                  key={`out-${portTraffic.port}`}
                  type="monotone"
                  dataKey={`Out (Port ${portTraffic.port})`}
                  stroke="#10b981"
                  dot={false}
                />
              </>
            ))}
          </LineChart>
        </ResponsiveContainer>
      ) : (
        <p className="text-gray-500 text-center py-8">No traffic data available</p>
      )}
    </div>
  );
};
