import { useQuery } from '@tanstack/react-query';
import { clientsApi } from '../api/client';
import type { Client } from '../types';
import { useMediaQuery } from '../hooks/useMediaQuery';

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

interface ClientListProps {
  onSelectClient?: (name: string) => void;
}

// Group clients by hostname
function groupClientsByHostname(clients: Client[]): Map<string, Client[]> {
  const groups = new Map<string, Client[]>();

  for (const client of clients) {
    const hostname = client.hostname || 'Unknown';
    if (!groups.has(hostname)) {
      groups.set(hostname, []);
    }
    groups.get(hostname)!.push(client);
  }

  return groups;
}

const ClientCard = ({ client, onSelectClient }: {
  client: Client;
  onSelectClient?: (name: string) => void;
}) => (
  <div className="bg-gray-50 dark:bg-slate-700/50 border border-gray-200 dark:border-slate-700 rounded-lg p-4">
    <div className="flex items-center justify-between mb-2">
      <span className="text-sm font-semibold text-gray-900 dark:text-slate-100">{client.name}</span>
      <span
        className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ${
          client.online
            ? 'bg-emerald-500/10 text-emerald-500'
            : 'bg-muted text-muted-foreground'
        }`}
      >
        {client.online ? 'online' : 'offline'}
      </span>
    </div>
    <div className="grid grid-cols-2 gap-2 text-xs text-gray-500 dark:text-slate-400 mb-3">
      <span>Hostname: {client.hostname ?? 'N/A'}</span>
      <span>Version: {client.client_version ?? 'N/A'}</span>
      <span>Referenced: {client.referenced_by_rules}</span>
      <span>Last seen: {new Date(client.last_seen_at).toLocaleString()}</span>
    </div>
    <div className="flex justify-end">
      <button
        onClick={() => onSelectClient?.(client.name)}
        className="text-blue-600 dark:text-blue-400 hover:text-blue-900 dark:hover:text-blue-300 text-sm font-medium"
      >
        Details
      </button>
    </div>
  </div>
);

export const ClientList = ({ onSelectClient }: ClientListProps) => {
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const { data: clients = [], isLoading } = useQuery({
    queryKey: ['clients', 'list-component'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });

  if (isLoading) {
    return (
      <div className="bg-white dark:bg-slate-800 p-6 rounded-lg shadow dark:shadow-slate-950/20">
        <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">Connected Clients</h3>
        <p className="text-gray-500 dark:text-slate-400">Loading...</p>
      </div>
    );
  }

  const clientGroups = Array.from(groupClientsByHostname(clients).entries())
    .map(([hostname, clients]) => ({ hostname, clients }))
    .sort((a, b) => a.hostname.localeCompare(b.hostname));

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
                    ({group.clients.length} client{group.clients.length !== 1 ? 's' : ''})
                  </span>
                </h4>
              </div>
              {isSmallScreen ? (
                <div className="p-4 space-y-3">
                  {group.clients.map((client) => (
                    <ClientCard
                      key={client.name}
                      client={client}
                      onSelectClient={onSelectClient}
                    />
                  ))}
                </div>
              ) : (
                <div className="overflow-x-auto">
                  <table className="min-w-full divide-y divide-gray-200 dark:divide-slate-700">
                    <thead className="bg-gray-50 dark:bg-slate-700/50">
                      <tr>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Name
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Status
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Version
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Referenced
                        </th>
                        <th className="px-4 py-3 text-right text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          Actions
                        </th>
                      </tr>
                    </thead>
                    <tbody className="bg-white dark:bg-slate-800 divide-y divide-gray-200 dark:divide-slate-700">
                      {group.clients.map((client) => (
                        <tr key={client.name} className="hover:bg-gray-50 dark:hover:bg-slate-700/50">
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm font-medium text-gray-900 dark:text-slate-100">{client.name}</span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span
                              className={`inline-flex items-center rounded-full px-2 py-1 text-xs font-medium ${
                                client.online
                                  ? 'bg-emerald-500/10 text-emerald-500'
                                  : 'bg-muted text-muted-foreground'
                              }`}
                            >
                              {client.online ? 'online' : 'offline'}
                            </span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">
                              {client.client_version ?? 'N/A'}
                            </span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">{client.referenced_by_rules}</span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap text-right text-sm font-medium">
                            <button
                              onClick={() => onSelectClient?.(client.name)}
                              className="text-blue-600 dark:text-blue-400 hover:text-blue-900 dark:hover:text-blue-300"
                            >
                              Details
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
