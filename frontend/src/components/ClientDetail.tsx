import { useQuery } from 'react-query';
import { getPortTraffic } from '../api/client';
import type { PortTraffic } from '../types';
import { TrafficChart } from './TrafficChart';

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

const formatTime = (timestamp: string): string => {
  const date = new Date(timestamp);
  return date.toLocaleTimeString();
};

export const ClientDetail = ({ port, onClose }: ClientDetailProps) => {
  const { data: traffic, isLoading } = useQuery<PortTraffic>(
    ['portTraffic', port],
    () => getPortTraffic(port),
    {
      refetchInterval: 5000,
    }
  );

  const singlePortTraffic = traffic ? [traffic] : [];

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
          ) : traffic ? (
            <div className="space-y-6">
              {/* Traffic summary */}
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
              <div>
                <h3 className="text-lg font-medium text-gray-900 mb-4">Traffic History</h3>
                <TrafficChart traffic={singlePortTraffic} />
              </div>

              {/* Recent buckets */}
              {traffic.buckets.length > 0 && (
                <div>
                  <h3 className="text-lg font-medium text-gray-900 mb-4">Recent Activity</h3>
                  <div className="bg-gray-50 rounded-lg overflow-hidden">
                    <table className="min-w-full divide-y divide-gray-200">
                      <thead className="bg-gray-100">
                        <tr>
                          <th className="px-4 py-2 text-left text-xs font-medium text-gray-500 uppercase">
                            Time
                          </th>
                          <th className="px-4 py-2 text-right text-xs font-medium text-gray-500 uppercase">
                            Bytes In
                          </th>
                          <th className="px-4 py-2 text-right text-xs font-medium text-gray-500 uppercase">
                            Bytes Out
                          </th>
                        </tr>
                      </thead>
                      <tbody className="divide-y divide-gray-200">
                        {traffic.buckets.slice(-10).reverse().map((bucket, index) => (
                          <tr key={index} className="bg-white">
                            <td className="px-4 py-2 text-sm text-gray-900">
                              {formatTime(bucket.timestamp)}
                            </td>
                            <td className="px-4 py-2 text-sm text-gray-500 text-right">
                              {formatBytes(bucket.bytes_in)}
                            </td>
                            <td className="px-4 py-2 text-sm text-gray-500 text-right">
                              {formatBytes(bucket.bytes_out)}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              )}
            </div>
          ) : (
            <p className="text-gray-500 text-center py-8">No traffic data available</p>
          )}
        </div>
      </div>
    </div>
  );
};
