import { useQuery } from 'react-query';
import { getAllQuality, getQualityWarnings } from '../api/client';
import type { ClientWithQuality } from '../types';
import { getQualityColor, getQualityText } from './ClientList';

const formatMs = (value: number): string => `${value.toFixed(1)} ms`;

const formatPercent = (value: number): string => `${(value * 100).toFixed(1)}%`;

// Quality heatmap cell
const QualityHeatmapCell = ({ client }: { client: ClientWithQuality }) => {
  const color = getQualityColor(client.quality.quality_score);
  const bgOpacity = (client.quality.quality_score / 100) * 0.3 + 0.1;

  return (
    <div
      className="p-3 rounded-lg border border-gray-200 hover:shadow-md transition-shadow cursor-pointer"
      style={{ backgroundColor: `${color}${Math.floor(bgOpacity * 100).toString(16).padStart(2, '0')}` }}
    >
      <div className="flex items-center justify-between">
        <div>
          <span className="text-sm font-semibold text-gray-800">Port {client.port}</span>
          {client.hostname && (
            <p className="text-xs text-gray-500 mt-1">{client.hostname}</p>
          )}
        </div>
        <div className="text-right">
          <span className="text-sm font-bold" style={{ color }}>
            {client.quality.quality_score}
          </span>
        </div>
      </div>
      <div className="mt-2 flex justify-between text-xs text-gray-600">
        <span>RTT: {formatMs(client.quality.avg_rtt_ms)}</span>
        <span>Loss: {formatPercent(client.quality.loss_rate)}</span>
      </div>
    </div>
  );
};

// Worst connections table
const WorstConnectionsTable = ({ clients }: { clients: ClientWithQuality[] }) => {
  const worstClients = [...clients]
    .sort((a, b) => a.quality.quality_score - b.quality.quality_score)
    .slice(0, 10);

  return (
    <div className="bg-white rounded-lg shadow overflow-hidden">
      <div className="px-6 py-4 border-b border-gray-200">
        <h3 className="text-lg font-medium text-gray-900">Worst Connections (by Quality Score)</h3>
      </div>
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">Rank</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">Port</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">Score</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">RTT</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">Loss</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase">Status</th>
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {worstClients.map((client, index) => {
              const color = getQualityColor(client.quality.quality_score);
              return (
                <tr key={client.port} className="hover:bg-gray-50">
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                    #{index + 1}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">
                    {client.port}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap" style={{ color }}>
                    <span className="font-semibold">{client.quality.quality_score}</span>
                    <span className="text-xs ml-2">({getQualityText(client.quality.quality_score)})</span>
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                    {formatMs(client.quality.avg_rtt_ms)}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                    {formatPercent(client.quality.loss_rate)}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap">
                    {client.quality.is_critical && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-red-100 text-red-800">
                        Critical
                      </span>
                    )}
                    {client.quality.is_warning && !client.quality.is_critical && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-yellow-100 text-yellow-800">
                        Warning
                      </span>
                    )}
                    {!client.quality.is_critical && !client.quality.is_warning && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-green-100 text-green-800">
                        Healthy
                      </span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
};

// Aggregate quality metrics
const QualityMetrics = ({ clients }: { clients: ClientWithQuality[] }) => {
  const avgScore = clients.length > 0
    ? Math.round(clients.reduce((sum, c) => sum + c.quality.quality_score, 0) / clients.length)
    : 0;
  const warningCount = clients.filter(c => c.quality.is_warning).length;
  const criticalCount = clients.filter(c => c.quality.is_critical).length;

  return (
    <div className="grid grid-cols-1 gap-5 sm:grid-cols-2 lg:grid-cols-4 mb-6">
      <div className="bg-white overflow-hidden shadow rounded-lg p-6">
        <div className="flex items-center">
          <div className="flex-shrink-0 rounded-md p-3" style={{ backgroundColor: `${getQualityColor(avgScore)}30` }}>
            <svg className="h-6 w-6" style={{ color: getQualityColor(avgScore) }} fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 10V3L4 14h7v7l9-11h-7z" />
            </svg>
          </div>
          <div className="ml-5 w-0 flex-1">
            <dl>
              <dt className="text-sm font-medium text-gray-500 truncate">Avg Quality Score</dt>
              <dd className="text-lg font-semibold" style={{ color: getQualityColor(avgScore) }}>
                {avgScore} ({getQualityText(avgScore)})
              </dd>
            </dl>
          </div>
        </div>
      </div>

      <div className="bg-white overflow-hidden shadow rounded-lg p-6">
        <div className="flex items-center">
          <div className="flex-shrink-0 bg-blue-500 rounded-md p-3">
            <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
            </svg>
          </div>
          <div className="ml-5 w-0 flex-1">
            <dl>
              <dt className="text-sm font-medium text-gray-500 truncate">Clients Monitored</dt>
              <dd className="text-lg font-semibold text-gray-900">{clients.length}</dd>
            </dl>
          </div>
        </div>
      </div>

      <div className="bg-white overflow-hidden shadow rounded-lg p-6">
        <div className="flex items-center">
          <div className="flex-shrink-0 bg-yellow-500 rounded-md p-3">
            <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
            </svg>
          </div>
          <div className="ml-5 w-0 flex-1">
            <dl>
              <dt className="text-sm font-medium text-gray-500 truncate">Warnings</dt>
              <dd className="text-lg font-semibold text-yellow-600">{warningCount}</dd>
            </dl>
          </div>
        </div>
      </div>

      <div className="bg-white overflow-hidden shadow rounded-lg p-6">
        <div className="flex items-center">
          <div className="flex-shrink-0 bg-red-500 rounded-md p-3">
            <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
          </div>
          <div className="ml-5 w-0 flex-1">
            <dl>
              <dt className="text-sm font-medium text-gray-500 truncate">Critical</dt>
              <dd className="text-lg font-semibold text-red-600">{criticalCount}</dd>
            </dl>
          </div>
        </div>
      </div>
    </div>
  );
};

interface QualityPageProps {
  onSelectClient?: (port: number) => void;
}

export const QualityPage = ({ onSelectClient }: QualityPageProps) => {
  const { data: qualityData = [], isLoading } = useQuery<ClientWithQuality[]>(
    'allQuality',
    getAllQuality,
    {
      refetchInterval: 5000,
    }
  );

  // Warnings query for future use
  useQuery(
    'qualityWarnings',
    getQualityWarnings,
    {
      refetchInterval: 5000,
    }
  );

  // Group by hostname for heatmap
  const groupedByHostname = qualityData.reduce((acc, client) => {
    const hostname = client.hostname || 'Unknown';
    if (!acc.has(hostname)) {
      acc.set(hostname, []);
    }
    acc.get(hostname)!.push(client);
    return acc;
  }, new Map<string, ClientWithQuality[]>());

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-16">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <QualityMetrics clients={qualityData} />

      {/* Quality Heatmap by Hostname */}
      {Array.from(groupedByHostname.entries()).map(([hostname, clients]) => (
        <div key={hostname} className="bg-white rounded-lg shadow p-6">
          <h3 className="text-lg font-medium text-gray-900 mb-4">
            {hostname} ({clients.length} {clients.length === 1 ? 'connection' : 'connections'})
          </h3>
          <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 gap-4">
            {clients.map(client => (
              <div key={client.port} onClick={() => onSelectClient?.(client.port)}>
                <QualityHeatmapCell client={client} />
              </div>
            ))}
          </div>
        </div>
      ))}

      {/* Worst Connections Table */}
      {qualityData.length > 0 && <WorstConnectionsTable clients={qualityData} />}
    </div>
  );
};
