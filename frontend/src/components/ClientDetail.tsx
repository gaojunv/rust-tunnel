import { useQuery } from 'react-query';
import { getPortTraffic, getPortQuality } from '../api/client';
import type { PortTraffic, PortQualityResponse, QualitySample } from '../types';
import { TrafficChart } from './TrafficChart';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, BarChart, Bar } from 'recharts';
import { getQualityColor, getQualityText } from './ClientList';

interface ClientDetailProps {
  port: number;
  onClose: () => void;
}

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

const formatMs = (value: number): string => `${value.toFixed(1)} ms`;

const formatPercent = (value: number): string => `${(value * 100).toFixed(1)}%`;

// Quality gauge component
const QualityGauge = ({ score }: { score: number }) => {
  const color = getQualityColor(score);
  const circumference = 2 * Math.PI * 45;
  const strokeDashoffset = circumference - (score / 100) * circumference;

  return (
    <div className="flex flex-col items-center">
      <div className="relative">
        <svg width="120" height="120" className="transform -rotate-90">
          <circle
            cx="60"
            cy="60"
            r="45"
            stroke="#e5e7eb"
            strokeWidth="10"
            fill="none"
          />
          <circle
            cx="60"
            cy="60"
            r="45"
            stroke={color}
            strokeWidth="10"
            fill="none"
            strokeDasharray={circumference}
            strokeDashoffset={strokeDashoffset}
            strokeLinecap="round"
            style={{ transition: 'stroke-dashoffset 0.5s ease' }}
          />
        </svg>
        <div className="absolute inset-0 flex flex-col items-center justify-center">
          <span className="text-2xl font-bold" style={{ color }}>
            {score}
          </span>
          <span className="text-xs text-gray-500">
            {getQualityText(score)}
          </span>
        </div>
      </div>
    </div>
  );
};

// RTT chart component
const RTTChart = ({ samples }: { samples: QualitySample[] }) => {
  const chartData = samples.map(sample => ({
    time: new Date(sample.timestamp).toLocaleTimeString(),
    avg_rtt_ms: sample.avg_rtt_ms,
  }));

  return (
    <div>
      <h4 className="text-sm font-medium text-gray-700 mb-2">RTT History (Last 60 min)</h4>
      {chartData.length > 0 ? (
        <ResponsiveContainer width="100%" height={200}>
          <LineChart data={chartData}>
            <CartesianGrid strokeDasharray="3 3" />
            <XAxis dataKey="time" tick={{ fontSize: 10 }} />
            <YAxis tick={{ fontSize: 10 }} />
            <Tooltip formatter={(value: number) => formatMs(value)} />
            <Line
              type="monotone"
              dataKey="avg_rtt_ms"
              name="Avg RTT"
              stroke="#3b82f6"
              dot={false}
              strokeWidth={2}
            />
          </LineChart>
        </ResponsiveContainer>
      ) : (
        <p className="text-gray-500 text-center py-4 text-sm">No RTT data available</p>
      )}
    </div>
  );
};

// Loss rate chart component
const LossChart = ({ samples }: { samples: QualitySample[] }) => {
  const chartData = samples.map(sample => ({
    time: new Date(sample.timestamp).toLocaleTimeString(),
    loss_rate: sample.loss_rate * 100,
  }));

  return (
    <div>
      <h4 className="text-sm font-medium text-gray-700 mb-2">Packet Loss History (Last 60 min)</h4>
      {chartData.length > 0 ? (
        <ResponsiveContainer width="100%" height={200}>
          <BarChart data={chartData}>
            <CartesianGrid strokeDasharray="3 3" />
            <XAxis dataKey="time" tick={{ fontSize: 10 }} />
            <YAxis tick={{ fontSize: 10 }} unit="%" />
            <Tooltip formatter={(value: number) => `${value.toFixed(2)}%`} />
            <Bar
              dataKey="loss_rate"
              name="Loss Rate"
              fill="#ef4444"
              radius={[2, 2, 0, 0]}
            />
          </BarChart>
        </ResponsiveContainer>
      ) : (
        <p className="text-gray-500 text-center py-4 text-sm">No loss data available</p>
      )}
    </div>
  );
};

export const ClientDetail = ({ port, onClose }: ClientDetailProps) => {
  const { data: traffic, isLoading: isLoadingTraffic } = useQuery<PortTraffic>(
    ['portTraffic', port],
    () => getPortTraffic(port),
    {
      refetchInterval: 5000,
    }
  );

  const { data: quality, isLoading: isLoadingQuality } = useQuery<PortQualityResponse>(
    ['portQuality', port],
    () => getPortQuality(port),
    {
      refetchInterval: 5000,
    }
  );

  const singlePortTraffic = traffic ? [traffic] : [];
  const isLoading = isLoadingTraffic && isLoadingQuality;

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center p-4 z-50">
      <div className="bg-white rounded-lg shadow-xl max-w-2xl w-full max-h-[90vh] overflow-hidden">
        <div className="flex items-center justify-between p-6 border-b">
          <h2 className="text-xl font-semibold text-gray-900">
            Client Details - Port {port}
          </h2>
          <button
            onClick={onClose}
            className="text-gray-400 hover:text-gray-600"
          >
            <svg className="h-6 w-6" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>

        <div className="p-6 overflow-y-auto max-h-[calc(90vh-80px)]">
          {isLoading ? (
            <p className="text-gray-500 text-center py-8">Loading...</p>
          ) : (
            <div className="space-y-6">
              {/* Quality Summary */}
              {quality && (
                <div>
                  <h3 className="text-lg font-medium text-gray-900 mb-4">Connection Quality</h3>
                  <div className="grid grid-cols-2 gap-4">
                    <div className="bg-gray-50 p-4 rounded-lg flex items-center justify-center">
                      <QualityGauge score={quality.current.quality_score} />
                    </div>
                    <div className="grid grid-cols-2 gap-2">
                      <div className="bg-blue-50 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-blue-600">Avg RTT</dt>
                        <dd className="text-lg font-semibold text-blue-900">
                          {formatMs(quality.current.avg_rtt_ms)}
                        </dd>
                      </div>
                      <div className="bg-red-50 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-red-600">Loss Rate</dt>
                        <dd className="text-lg font-semibold text-red-900">
                          {formatPercent(quality.current.loss_rate)}
                        </dd>
                      </div>
                      <div className="bg-green-50 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-green-600">Min RTT</dt>
                        <dd className="text-lg font-semibold text-green-900">
                          {formatMs(quality.current.min_rtt_ms)}
                        </dd>
                      </div>
                      <div className="bg-orange-50 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-orange-600">Max RTT</dt>
                        <dd className="text-lg font-semibold text-orange-900">
                          {formatMs(quality.current.max_rtt_ms)}
                        </dd>
                      </div>
                    </div>
                  </div>

                  {/* Quality Charts */}
                  <div className="grid grid-cols-1 gap-4 mt-4">
                    <div className="bg-gray-50 p-4 rounded-lg">
                      <RTTChart samples={quality.history} />
                    </div>
                    <div className="bg-gray-50 p-4 rounded-lg">
                      <LossChart samples={quality.history} />
                    </div>
                  </div>
                </div>
              )}

              {/* Traffic summary */}
              {traffic && (
                <div>
                  <h3 className="text-lg font-medium text-gray-900 mb-4">Traffic Summary</h3>
                  <div className="grid grid-cols-2 gap-4">
                    <div className="bg-purple-50 p-4 rounded-lg">
                      <dt className="text-sm font-medium text-purple-600">Total Bytes In</dt>
                      <dd className="text-2xl font-semibold text-purple-900">
                        {formatBytes(traffic.total_bytes_in)}
                      </dd>
                    </div>
                    <div className="bg-orange-50 p-4 rounded-lg">
                      <dt className="text-sm font-medium text-orange-600">Total Bytes Out</dt>
                      <dd className="text-2xl font-semibold text-orange-900">
                        {formatBytes(traffic.total_bytes_out)}
                      </dd>
                    </div>
                  </div>

                  {/* Traffic chart */}
                  <div className="mt-4">
                    <h4 className="text-sm font-medium text-gray-700 mb-2">Traffic History</h4>
                    <TrafficChart traffic={singlePortTraffic} />
                  </div>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
