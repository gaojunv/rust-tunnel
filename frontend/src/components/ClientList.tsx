import { useQuery, useMutation, useQueryClient } from 'react-query';
import { getClients, disconnectClient } from '../api/client';
import type { ClientResponse, ClientGroup } from '../types';

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

export const ClientList = ({ onSelectClient }: ClientListProps) => {
  const queryClient = useQueryClient();

  const { data: clients = [], isLoading } = useQuery<ClientResponse[]>(
    'clients',
    getClients,
    {
      refetchInterval: 5000, // Refresh every 5 seconds
    }
  );

  const disconnectMutation = useMutation(
    (port: number) => disconnectClient(port),
    {
      onSuccess: () => {
        queryClient.invalidateQueries('clients');
        queryClient.invalidateQueries('traffic');
        queryClient.invalidateQueries('metrics');
      },
    }
  );

  const handleDisconnect = (port: number) => {
    if (confirm(`Are you sure you want to disconnect the client on port ${port}?`)) {
      disconnectMutation.mutate(port);
    }
  };

  if (isLoading) {
    return (
      <div className="bg-white p-6 rounded-lg shadow">
        <h3 className="text-lg font-medium text-gray-900 mb-4">Connected Clients</h3>
        <p className="text-gray-500">Loading...</p>
      </div>
    );
  }

  const clientGroups = groupClientsByHostname(clients);

  return (
    <div className="bg-white p-6 rounded-lg shadow">
      <h3 className="text-lg font-medium text-gray-900 mb-4">Connected Clients</h3>
      {clientGroups.length > 0 ? (
        <div className="space-y-6">
          {clientGroups.map((group) => (
            <div key={group.hostname} className="border border-gray-200 rounded-lg overflow-hidden">
              <div className="bg-gray-50 px-4 py-3 border-b border-gray-200">
                <h4 className="font-medium text-gray-900 flex items-center">
                  <svg className="w-5 h-5 mr-2 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                  </svg>
                  {group.hostname}
                  <span className="ml-2 text-sm font-normal text-gray-500">
                    ({group.clients.length} port{group.clients.length !== 1 ? 's' : ''})
                  </span>
                </h4>
              </div>
              <div className="overflow-x-auto">
                <table className="min-w-full divide-y divide-gray-200">
                  <thead className="bg-gray-50">
                    <tr>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                        Port
                      </th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                        Connections
                      </th>
                      <th className="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">
                        Actions
                      </th>
                    </tr>
                  </thead>
                  <tbody className="bg-white divide-y divide-gray-200">
                    {group.clients.map((client) => (
                      <tr key={client.port} className="hover:bg-gray-50">
                        <td className="px-6 py-4 whitespace-nowrap">
                          <span className="text-sm font-medium text-gray-900">{client.port}</span>
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <span className="text-sm text-gray-500">{client.connection_count}</span>
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-right text-sm font-medium">
                          <button
                            onClick={() => onSelectClient?.(client.port)}
                            className="text-blue-600 hover:text-blue-900 mr-4"
                          >
                            Details
                          </button>
                          <button
                            onClick={() => handleDisconnect(client.port)}
                            disabled={disconnectMutation.isLoading}
                            className="text-red-600 hover:text-red-900 disabled:opacity-50"
                          >
                            Disconnect
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          ))}
        </div>
      ) : (
        <p className="text-gray-500 text-center py-8">No clients connected</p>
      )}
    </div>
  );
};
