import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getClients, disconnectClient } from '../api/client';
import type { ClientResponse, ClientGroup, ConnectionQuality } from '../types';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { formatMs, formatPercent } from '../utils/format';

// Get quality score color
export const getQualityColor = (score: number): string => {
  if (score >= 90) return '#22c55e'; // Green
  if (score >= 70) return '#eab308'; // Yellow
  if (score >= 50) return '#f97316'; // Orange
  return '#ef4444'; // Red
};

// Get quality text description
export const getQualityText = (score: number): string => {
  if (score >= 90) return 'Excellent';
  if (score >= 70) return 'Good';
  if (score >= 50) return 'Fair';
  return 'Poor';
};

// Quality score indicator component
const QualityIndicator = ({ quality }: { quality: ConnectionQuality | undefined }) => {
  if (!quality) {
    return (
      <span className="inline-flex items-center">
        <span className="w-3 h-3 rounded-full bg-gray-300 mr-2"></span>
        <span className="text-gray-400 dark:text-slate-500 text-sm">N/A</span>
      </span>
    );
  }

  const color = getQualityColor(quality.quality_score);
  const text = getQualityText(quality.quality_score);
  const blinkClass = quality.is_critical ? 'animate-pulse' : quality.is_warning ? '' : '';

  return (
    <span className="inline-flex items-center">
      <span
        className={`w-3 h-3 rounded-full mr-2 ${blinkClass}`}
        style={{ backgroundColor: color }}
      ></span>
      <span className="text-sm font-medium" style={{ color }}>
        {text} ({quality.quality_score})
      </span>
    </span>
  );
};

interface ClientListProps {
  onSelectClient?: (port: number) => void;
}

// Group clients by hostname
function groupClientsByHostname(clients: ClientResponse[]): ClientGroup[] {
  const groups = new Map<string, ClientResponse[]>();

  for (const client of clients) {
    const hostname = client.hostname || 'Unknown';
    if (!groups.has(hostname)) {
      groups.set(hostname, []);
    }
    groups.get(hostname)!.push(client);
  }

  return Array.from(groups.entries())
    .map(([hostname, clients]) => ({ hostname, clients }))
    .sort((a, b) => a.hostname.localeCompare(b.hostname));
}

const ClientCard = ({ client, onSelectClient, onDisconnect, disabled }: {
  client: ClientResponse;
  onSelectClient?: (port: number) => void;
  onDisconnect: (port: number) => void;
  disabled: boolean;
}) => (
  <div className="bg-gray-50 dark:bg-slate-700/50 border border-gray-200 dark:border-slate-700 rounded-lg p-4">
    <div className="flex items-center justify-between mb-2">
      <span className="text-sm font-semibold text-gray-900 dark:text-slate-100">Port {client.port}</span>
      <QualityIndicator quality={client.quality} />
    </div>
    <div className="grid grid-cols-2 gap-2 text-xs text-gray-500 dark:text-slate-400 mb-3">
      <span>RTT: {client.quality ? formatMs(client.quality.avg_rtt_ms) : 'N/A'}</span>
      <span>Loss: {client.quality ? formatPercent(client.quality.loss_rate) : 'N/A'}</span>
      <span>Connections: {client.connection_count}</span>
    </div>
    <div className="flex justify-end space-x-3">
      <button
        onClick={() => onSelectClient?.(client.port)}
        className="text-blue-600 dark:text-blue-400 hover:text-blue-900 dark:hover:text-blue-300 text-sm font-medium"
      >
        Details
      </button>
      <button
        onClick={() => onDisconnect(client.port)}
        disabled={disabled}
        className="text-red-600 dark:text-red-400 hover:text-red-900 dark:hover:text-red-300 text-sm font-medium disabled:opacity-50"
      >
        Disconnect
      </button>
    </div>
  </div>
);

export const ClientList = ({ onSelectClient }: ClientListProps) => {
  const queryClient = useQueryClient();
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const { data: clients = [], isLoading } = useQuery<ClientResponse[]>({
    queryKey: ['clients'],
    queryFn: getClients,
    refetchInterval: 5000, // Refresh every 5 seconds
  });

  const disconnectMutation = useMutation({
    mutationFn: (port: number) => disconnectClient(port),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['clients'] });
      queryClient.invalidateQueries({ queryKey: ['traffic'] });
      queryClient.invalidateQueries({ queryKey: ['metrics'] });
    },
  });

  const handleDisconnect = (port: number) => {
    if (confirm(`Are you sure you want to disconnect the client on port ${port}?`)) {
      disconnectMutation.mutate(port);
    }
  };

  if (isLoading) {
    return (
      <div className="bg-white dark:bg-slate-800 p-6 rounded-lg shadow dark:shadow-slate-950/20">
        <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">Connected Clients</h3>
        <p className="text-gray-500 dark:text-slate-400">Loading...</p>
      </div>
    );
  }

  const clientGroups = groupClientsByHostname(clients);

  return (
    <div className="bg-white dark:bg-slate-800 p-6 rounded-lg shadow dark:shadow-slate-950/20">
      <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">Connected Clients</h3>
      {clientGroups.length > 0 ? (
        <div className="space-y-6">
          {clientGroups.map((group) => (
            <div key={group.hostname} className="border border-gray-200 dark:border-slate-700 rounded-lg overflow-hidden">
              <div className="bg-gray-50 dark:bg-slate-700/50 px-4 py-3 border-b border-gray-200 dark:border-slate-700">
                <h4 className="font-medium text-gray-900 dark:text-slate-100 flex items-center">
                  <svg className="w-5 h-5 mr-2 text-gray-500 dark:text-slate-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                  </svg>
                  {group.hostname}
                  <span className="ml-2 text-sm font-normal text-gray-500 dark:text-slate-400">
                    ({group.clients.length} port{group.clients.length !== 1 ? 's' : ''})
                  </span>
                </h4>
              </div>
              {isSmallScreen ? (
                <div className="p-4 space-y-3">
                  {group.clients.map((client) => (
                    <ClientCard
                      key={client.port}
                      client={client}
                      onSelectClient={onSelectClient}
                      onDisconnect={handleDisconnect}
                      disabled={disconnectMutation.isPending}
                    />
                  ))}
                </div>
              ) : (
                <div className="overflow-x-auto">
                  <table className="min-w-full divide-y divide-gray-200 dark:divide-slate-700">
                    <thead className="bg-gray-50 dark:bg-slate-700/50">
                      <tr>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Port
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Quality
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          RTT (ms)
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Loss (%)
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Connections
                        </th>
                        <th className="px-4 py-3 text-right text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Actions
                        </th>
                      </tr>
                    </thead>
                    <tbody className="bg-white dark:bg-slate-800 divide-y divide-gray-200 dark:divide-slate-700">
                      {group.clients.map((client) => (
                        <tr key={client.port} className="hover:bg-gray-50 dark:hover:bg-slate-700/50">
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm font-medium text-gray-900 dark:text-slate-100">{client.port}</span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <QualityIndicator quality={client.quality} />
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">
                              {client.quality ? formatMs(client.quality.avg_rtt_ms) : 'N/A'}
                            </span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">
                              {client.quality ? formatPercent(client.quality.loss_rate) : 'N/A'}
                            </span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">{client.connection_count}</span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap text-right text-sm font-medium">
                            <button
                              onClick={() => onSelectClient?.(client.port)}
                              className="text-blue-600 dark:text-blue-400 hover:text-blue-900 dark:hover:text-blue-300 mr-4"
                            >
                              Details
                            </button>
                            <button
                              onClick={() => handleDisconnect(client.port)}
                              disabled={disconnectMutation.isPending}
                              className="text-red-600 dark:text-red-400 hover:text-red-900 dark:hover:text-red-300 disabled:opacity-50"
                            >
                              Disconnect
                            </button>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </div>
          ))}
        </div>
      ) : (
        <p className="text-gray-500 dark:text-slate-400 text-center py-8">No clients connected</p>
      )}
    </div>
  );
};
