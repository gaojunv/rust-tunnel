import { useState } from 'react';
import { useQuery } from 'react-query';
import { Navbar } from './Navbar';
import { ClientList } from './ClientList';
import { TrafficChart } from './TrafficChart';
import { ClientDetail } from './ClientDetail';
import { QualityPage } from './QualityPage';
import { getMetrics, getTraffic } from '../api/client';

interface DashboardProps {
  onLogout: () => void;
}

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

export const Dashboard = ({ onLogout }: DashboardProps) => {
  const [selectedPort, setSelectedPort] = useState<number | null>(null);
  const [activeTab, setActiveTab] = useState<'dashboard' | 'quality'>('dashboard');

  const { data: metrics } = useQuery('metrics', getMetrics, {
    refetchInterval: 5000,
  });

  const { data: traffic = [] } = useQuery('traffic', getTraffic, {
    refetchInterval: 5000,
  });

  const handleSelectClient = (port: number) => {
    setSelectedPort(port);
    // Switch to dashboard tab when selecting a client from quality page
    if (activeTab === 'quality') {
      setActiveTab('dashboard');
    }
  };

  return (
    <div className="min-h-screen bg-gray-100">
      <Navbar onLogout={onLogout} activeTab={activeTab} onTabChange={setActiveTab} />
      <main className="max-w-7xl mx-auto py-6 sm:px-6 lg:px-8">
        {activeTab === 'dashboard' ? (
          <>
            {/* Metrics cards */}
            <div className="grid grid-cols-1 gap-5 sm:grid-cols-2 lg:grid-cols-4 mb-6">
              <div className="bg-white overflow-hidden shadow rounded-lg p-6">
                <div className="flex items-center">
                  <div className="flex-shrink-0 bg-blue-500 rounded-md p-3">
                    <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
                    </svg>
                  </div>
                  <div className="ml-5 w-0 flex-1">
                    <dl>
                      <dt className="text-sm font-medium text-gray-500 truncate">Connected Clients</dt>
                      <dd className="text-lg font-semibold text-gray-900">{metrics?.client_count || 0}</dd>
                    </dl>
                  </div>
                </div>
              </div>

              <div className="bg-white overflow-hidden shadow rounded-lg p-6">
                <div className="flex items-center">
                  <div className="flex-shrink-0 bg-green-500 rounded-md p-3">
                    <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 7h12m0 0l-4-4m4 4l-4 4m0 6H4m0 0l4 4m-4-4l4-4" />
                    </svg>
                  </div>
                  <div className="ml-5 w-0 flex-1">
                    <dl>
                      <dt className="text-sm font-medium text-gray-500 truncate">Active Connections</dt>
                      <dd className="text-lg font-semibold text-gray-900">{metrics?.active_connection_count || 0}</dd>
                    </dl>
                  </div>
                </div>
              </div>

              <div className="bg-white overflow-hidden shadow rounded-lg p-6">
                <div className="flex items-center">
                  <div className="flex-shrink-0 bg-purple-500 rounded-md p-3">
                    <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M7 16l-4-4m0 0l4-4m-4 4h18" />
                    </svg>
                  </div>
                  <div className="ml-5 w-0 flex-1">
                    <dl>
                      <dt className="text-sm font-medium text-gray-500 truncate">Total Bytes In</dt>
                      <dd className="text-lg font-semibold text-gray-900">{formatBytes(metrics?.total_bytes_in || 0)}</dd>
                    </dl>
                  </div>
                </div>
              </div>

              <div className="bg-white overflow-hidden shadow rounded-lg p-6">
                <div className="flex items-center">
                  <div className="flex-shrink-0 bg-orange-500 rounded-md p-3">
                    <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 8l4 4m0 0l-4 4m4-4H3" />
                    </svg>
                  </div>
                  <div className="ml-5 w-0 flex-1">
                    <dl>
                      <dt className="text-sm font-medium text-gray-500 truncate">Total Bytes Out</dt>
                      <dd className="text-lg font-semibold text-gray-900">{formatBytes(metrics?.total_bytes_out || 0)}</dd>
                    </dl>
                  </div>
                </div>
              </div>
            </div>

            <div className="space-y-6">
              <ClientList onSelectClient={handleSelectClient} />
              <TrafficChart traffic={traffic} />
            </div>
          </>
        ) : (
          <QualityPage onSelectClient={handleSelectClient} />
        )}
      </main>

      {selectedPort !== null && (
        <ClientDetail
          port={selectedPort}
          onClose={() => setSelectedPort(null)}
        />
      )}
    </div>
  );
};
