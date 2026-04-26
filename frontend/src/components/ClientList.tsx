import { useQuery, useMutation, useQueryClient } from 'react-query';
import { getClients, disconnectClient } from '../api/client';
import type { ClientResponse } from '../types';

interface ClientListProps {
  onSelectClient?: (port: number) => void;
}

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

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

  return (
    <div className="bg-white p-6 rounded-lg shadow">
      <h3 className="text-lg font-medium text-gray-900 mb-4">Connected Clients</h3>
      {clients.length > 0 ? (
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
              {clients.map((client) => (
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
      ) : (
        <p className="text-gray-500 text-center py-8">No clients connected</p>
      )}
    </div>
  );
};
