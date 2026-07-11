import { useState } from 'react';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getDnsRecords, addDnsRecord, deleteDnsRecord } from '../api/client';

export const DnsPage: React.FC = () => {
  const [showAddForm, setShowAddForm] = useState(false);
  const [newName, setNewName] = useState('');
  const [newValue, setNewValue] = useState('');
  const [newPort, setNewPort] = useState(80);
  const queryClient = useQueryClient();

  const { data: records, isLoading } = useQuery({
    queryKey: ['dns-records'],
    queryFn: getDnsRecords,
    refetchInterval: 15000,
  });

  const addMutation = useMutation({
    mutationFn: (data: { name: string; record_type: string; value: string; port?: number }) =>
      addDnsRecord(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
      setShowAddForm(false);
      setNewName('');
      setNewValue('');
      setNewPort(80);
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (name: string) => deleteDnsRecord(name),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['dns-records'] }),
  });

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex justify-between items-center">
        <h2 className="text-2xl font-bold text-gray-800 dark:text-slate-100">DNS Records</h2>
        <button
          onClick={() => setShowAddForm(!showAddForm)}
          className="px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 transition-colors"
        >
          {showAddForm ? 'Cancel' : 'Add Record'}
        </button>
      </div>

      {showAddForm && (
        <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 p-6">
          <h3 className="text-lg font-semibold dark:text-slate-100 mb-4">Add DNS Record</h3>
          <div className="space-y-4">
            <div>
              <label className="block text-sm text-gray-600 dark:text-slate-300 mb-1">Domain Name</label>
              <input
                type="text"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                placeholder="e.g. myapp.tunnel.local"
                className="w-full px-3 py-2 border border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder-slate-500 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <div>
              <label className="block text-sm text-gray-600 dark:text-slate-300 mb-1">IP Address</label>
              <input
                type="text"
                value={newValue}
                onChange={(e) => setNewValue(e.target.value)}
                placeholder="e.g. 10.0.0.1"
                className="w-full px-3 py-2 border border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder-slate-500 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <div>
              <label className="block text-sm text-gray-600 dark:text-slate-300 mb-1">Port</label>
              <input
                type="number"
                value={newPort}
                onChange={(e) => setNewPort(Number(e.target.value))}
                className="w-full px-3 py-2 border border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <button
              onClick={() =>
                addMutation.mutate({
                  name: newName,
                  record_type: 'A',
                  value: newValue,
                  port: newPort,
                })
              }
              disabled={!newName || !newValue || addMutation.isPending}
              className="px-4 py-2 bg-green-600 text-white rounded-lg hover:bg-green-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {addMutation.isPending ? 'Adding...' : 'Add'}
            </button>
          </div>
        </div>
      )}

      <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 overflow-hidden">
        <table className="min-w-full">
          <thead className="bg-gray-50 dark:bg-slate-700/50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                Domain
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                Type
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                Value
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider">
                Actions
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200 dark:divide-slate-700 bg-white dark:bg-slate-800">
            {(!records || records.length === 0) ? (
              <tr>
                <td colSpan={4} className="px-6 py-12 text-center text-gray-500 dark:text-slate-400">
                  No DNS records found
                </td>
              </tr>
            ) : (
              records.map((record, idx) => (
                <tr key={idx} className="hover:bg-gray-50 dark:hover:bg-slate-700/50">
                  <td className="px-6 py-4 text-sm font-medium text-gray-800 dark:text-slate-100">
                    {record.name}
                  </td>
                  <td className="px-6 py-4 text-sm">
                    <span className="px-2 py-1 bg-gray-100 dark:bg-slate-700 rounded text-gray-600 dark:text-slate-300">
                      {record.record_type}
                    </span>
                  </td>
                  <td className="px-6 py-4 text-sm text-gray-600 dark:text-slate-300">
                    {record.value}
                  </td>
                  <td className="px-6 py-4 text-sm">
                    <button
                      onClick={() => {
                        if (confirm(`Delete ${record.name}?`)) {
                          deleteMutation.mutate(record.name);
                        }
                      }}
                      className="text-red-600 dark:text-red-400 hover:text-red-800 dark:hover:text-red-300"
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};
