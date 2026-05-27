import { useQuery } from 'react-query';
import { getTrojanConfig, getTrojanStats, getTrojanQuality } from '../api/client';

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

export const TrojanPage = () => {
  const { data: config, isLoading: configLoading } = useQuery(
    'trojan-config',
    getTrojanConfig,
    { refetchInterval: 5000 }
  );

  const { data: stats, isLoading: statsLoading } = useQuery(
    'trojan-stats',
    getTrojanStats,
    { refetchInterval: 5000 }
  );

  useQuery(
    'trojan-quality',
    getTrojanQuality,
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
        <h2 className="text-lg font-semibold text-gray-900 mb-4">Trojan Configuration</h2>
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
            <label className="block text-sm font-medium text-gray-500">Fallback</label>
            <p className="text-lg font-semibold text-gray-900">{config?.fallback || 'N/A'}</p>
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
    </div>
  );
};
