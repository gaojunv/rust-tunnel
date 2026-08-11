import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { clientsApi } from '../api/client';
import type { Client } from '../types';
import { useMediaQuery } from '../hooks/useMediaQuery';

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
}) => {
  const { t } = useTranslation();
  return (
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
          {client.online ? t('common.status.online') : t('common.status.offline')}
        </span>
      </div>
      <div className="grid grid-cols-2 gap-2 text-xs text-gray-500 dark:text-slate-400 mb-3">
        {/* min-w-0 + break-words：长 hostname / lastSeen 不可断时也能在单元格内折行，避免撑破卡片 */}
        <span className="min-w-0 break-words">{t('clients.list.hostnameLabel')}: {client.hostname ?? t('clients.list.nA')}</span>
        <span className="min-w-0 break-words">{t('clients.list.versionLabel')}: {client.client_version ?? t('clients.list.nA')}</span>
        <span className="min-w-0 break-words">{t('clients.list.referencedLabel')}: {client.referenced_by_rules}</span>
        <span className="min-w-0 break-words">{t('clients.list.lastSeenLabel')}: {new Date(client.last_seen_at).toLocaleString()}</span>
      </div>
      <div className="flex justify-end">
        <button
          onClick={() => onSelectClient?.(client.name)}
          className="text-blue-600 dark:text-blue-400 hover:text-blue-900 dark:hover:text-blue-300 text-sm font-medium"
        >
          {t('clients.list.details')}
        </button>
      </div>
    </div>
  );
};

export const ClientList = ({ onSelectClient }: ClientListProps) => {
  const { t } = useTranslation();
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const { data: clients = [], isLoading } = useQuery({
    queryKey: ['clients', 'list-component'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });

  if (isLoading) {
    return (
      <div className="bg-white dark:bg-slate-800 p-6 rounded-lg shadow dark:shadow-slate-950/20">
        <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">{t('clients.list.connectedClients')}</h3>
        <p className="text-gray-500 dark:text-slate-400">{t('common.loading')}</p>
      </div>
    );
  }

  const clientGroups = Array.from(groupClientsByHostname(clients).entries())
    .map(([hostname, clients]) => ({ hostname, clients }))
    .sort((a, b) => a.hostname.localeCompare(b.hostname));

  return (
    <div className="bg-white dark:bg-slate-800 p-6 rounded-lg shadow dark:shadow-slate-950/20">
      <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">{t('clients.list.connectedClients')}</h3>
      {clientGroups.length > 0 ? (
        <div className="space-y-6">
          {clientGroups.map((group) => (
            <div key={group.hostname} className="border border-gray-200 dark:border-slate-700 rounded-lg overflow-hidden">
              <div className="bg-gray-50 dark:bg-slate-700/50 px-4 py-3 border-b border-gray-200 dark:border-slate-700">
                <h4 className="font-medium text-gray-900 dark:text-slate-100 flex items-center">
                  <svg className="w-5 h-5 mr-2 text-gray-500 dark:text-slate-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                  </svg>
                  {group.hostname === 'Unknown' ? t('clients.list.unknown') : group.hostname}
                  <span className="ml-2 text-sm font-normal text-gray-500 dark:text-slate-400">
                    {t('clients.list.clientCount', { count: group.clients.length })}
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
                          {t('clients.table.name')}
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          {t('clients.table.status')}
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          {t('clients.table.version')}
                        </th>
                        <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          {t('clients.table.referenced')}
                        </th>
                        <th className="px-4 py-3 text-right text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                          {t('clients.table.actions')}
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
                              {client.online ? t('common.status.online') : t('common.status.offline')}
                            </span>
                          </td>
                          <td className="px-4 py-4 whitespace-nowrap">
                            <span className="text-sm text-gray-500 dark:text-slate-400">
                              {client.client_version ?? t('clients.list.nA')}
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
                              {t('clients.list.details')}
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
        <p className="text-gray-500 dark:text-slate-400 text-center py-8">{t('clients.list.noClients')}</p>
      )}
    </div>
  );
};
