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
  // Combine all traffic data
  const chartData = traffic.flatMap(portTraffic =>
    portTraffic.buckets.map(bucket => ({
      time: new Date(bucket.timestamp).toLocaleTimeString(),
      [`In (Port ${portTraffic.port})`]: bucket.bytes_in,
      [`Out (Port ${portTraffic.port})`]: bucket.bytes_out,
    }))
  );

  // Remove duplicates by time (simplified)
  const uniqueData = Array.from(new Map(chartData.map(d => [d.time, d])).values());

  return (
    <div className="bg-white p-6 rounded-lg shadow">
      <h3 className="text-lg font-medium text-gray-900 mb-4">Network Traffic</h3>
      {uniqueData.length > 0 ? (
        <ResponsiveContainer width="100%" height={300}>
          <LineChart data={uniqueData}>
            <CartesianGrid strokeDasharray="3 3" />
            <XAxis dataKey="time" />
            <YAxis tickFormatter={formatBytes} />
            <Tooltip formatter={(value: number) => formatBytes(value)} />
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
