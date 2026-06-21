import { useQuery } from 'react-query';
import { getAllQuality, getQualityWarnings } from '../api/client';
import type { ClientWithQuality } from '../types';
import { getQualityColor, getQualityText } from './ClientList';
import { formatMs, formatPercent } from '../utils/format';
import { StatCard } from './shared/StatCard';
import { useMediaQuery } from '../hooks/useMediaQuery';

// Quality heatmap cell
const QualityHeatmapCell = ({ client }: { client: ClientWithQuality }) => {
  const color = getQualityColor(client.quality.quality_score);
  const bgOpacity = (client.quality.quality_score / 100) * 0.3 + 0.1;

  return (
    <div
      className="p-3 rounded-lg border border-gray-200 dark:border-slate-700 hover:shadow-md transition-shadow cursor-pointer"
      style={{ backgroundColor: `${color}${Math.floor(bgOpacity * 100).toString(16).padStart(2, '0')}` }}
    >
      <div className="flex items-center justify-between">
        <div>
          <span className="text-sm font-semibold text-gray-800 dark:text-slate-100">Port {client.port}</span>
          {client.hostname && (
            <p className="text-xs text-gray-500 dark:text-slate-400 mt-1">{client.hostname}</p>
          )}
        </div>
        <div className="text-right">
          <span className="text-sm font-bold" style={{ color }}>
            {client.quality.quality_score}
          </span>
        </div>
      </div>
      <div className="mt-2 flex justify-between text-xs text-gray-600 dark:text-slate-300">
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
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  return (
    <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 overflow-hidden">
      <div className="px-6 py-4 border-b border-gray-200 dark:border-slate-700">
        <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100">Worst Connections (by Quality Score)</h3>
      </div>
      {isSmallScreen ? (
        <div className="p-4 grid grid-cols-1 gap-3">
          {worstClients.map((client, index) => {
            const color = getQualityColor(client.quality.quality_score);
            const statusBadge = client.quality.is_critical
              ? 'bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300'
              : client.quality.is_warning
              ? 'bg-yellow-100 text-yellow-800 dark:bg-yellow-900/40 dark:text-yellow-300'
              : 'bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300';
            const statusLabel = client.quality.is_critical ? 'Critical' : client.quality.is_warning ? 'Warning' : 'Healthy';
            return (
              <div key={client.port} className="bg-gray-50 dark:bg-slate-700/50 rounded-lg p-4 border border-gray-200 dark:border-slate-700">
                <div className="flex items-center justify-between mb-2">
                  <span className="text-sm font-semibold text-gray-900 dark:text-slate-100">#{index + 1} Port {client.port}</span>
                  <span className={`px-2 py-0.5 text-xs font-semibold rounded-full ${statusBadge}`}>{statusLabel}</span>
                </div>
                <div className="grid grid-cols-3 gap-2 text-xs text-gray-600 dark:text-slate-300">
                  <span>Score: <b style={{color}}>{client.quality.quality_score}</b></span>
                  <span>RTT: {formatMs(client.quality.avg_rtt_ms)}</span>
                  <span>Loss: {formatPercent(client.quality.loss_rate)}</span>
                </div>
              </div>
            );
          })}
        </div>
      ) : (
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200 dark:divide-slate-700">
          <thead className="bg-gray-50 dark:bg-slate-700/50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">Rank</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">Port</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">Score</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">RTT</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">Loss</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase">Status</th>
            </tr>
          </thead>
          <tbody className="bg-white dark:bg-slate-800 divide-y divide-gray-200 dark:divide-slate-700">
            {worstClients.map((client, index) => {
              const color = getQualityColor(client.quality.quality_score);
              return (
                <tr key={client.port} className="hover:bg-gray-50 dark:hover:bg-slate-700/50">
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500 dark:text-slate-400">
                    #{index + 1}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900 dark:text-slate-100">
                    {client.port}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap" style={{ color }}>
                    <span className="font-semibold">{client.quality.quality_score}</span>
                    <span className="text-xs ml-2 text-gray-500 dark:text-slate-400">({getQualityText(client.quality.quality_score)})</span>
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500 dark:text-slate-400">
                    {formatMs(client.quality.avg_rtt_ms)}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500 dark:text-slate-400">
                    {formatPercent(client.quality.loss_rate)}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap">
                    {client.quality.is_critical && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300">
                        Critical
                      </span>
                    )}
                    {client.quality.is_warning && !client.quality.is_critical && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-yellow-100 text-yellow-800 dark:bg-yellow-900/40 dark:text-yellow-300">
                        Warning
                      </span>
                    )}
                    {!client.quality.is_critical && !client.quality.is_warning && (
                      <span className="px-2 py-1 text-xs font-semibold rounded-full bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300">
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
      )}
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
      <StatCard
        label="Avg Quality Score"
        value={`${avgScore} (${getQualityText(avgScore)})`}
        color="blue"
        valueColor={getQualityColor(avgScore)}
        icon={
          <svg className="h-6 w-6" style={{ color: getQualityColor(avgScore) }} fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 10V3L4 14h7v7l9-11h-7z" />
          </svg>
        }
      />
      <StatCard
        label="Clients Monitored"
        value={`${clients.length}`}
        color="blue"
        icon={
          <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
          </svg>
        }
      />
      <StatCard
        label="Warnings"
        value={`${warningCount}`}
        color="yellow"
        valueColor="text-yellow-600 dark:text-yellow-400"
        icon={
          <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
          </svg>
        }
      />
      <StatCard
        label="Critical"
        value={`${criticalCount}`}
        color="red"
        valueColor="text-red-600 dark:text-red-400"
        icon={
          <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
          </svg>
        }
      />
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
        <div key={hostname} className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 p-6">
          <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">
            {hostname} ({clients.length} {clients.length === 1 ? 'connection' : 'connections'})
          </h3>
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
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
