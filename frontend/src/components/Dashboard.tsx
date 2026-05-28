import { useState } from 'react';
import { useQuery } from 'react-query';
import { Navbar } from './Navbar';
import { ClientList } from './ClientList';
import { TrafficChart } from './TrafficChart';
import { ClientDetail } from './ClientDetail';
import { QualityPage } from './QualityPage';
import { ShadowsocksPage } from './ShadowsocksPage';
import { TrojanPage } from './TrojanPage';
import { LogsPage } from './LogsPage';
import { MobileBottomNav } from './shared/MobileBottomNav';
import { StatCard } from './shared/StatCard';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { getMetrics, getTraffic } from '../api/client';
import { formatBytes } from '../utils/format';

interface DashboardProps {
  onLogout: () => void;
}

export const Dashboard = ({ onLogout }: DashboardProps) => {
  const [selectedPort, setSelectedPort] = useState<number | null>(null);
  const [activeTab, setActiveTab] = useState<'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs'>('dashboard');
  const isMobile = useMediaQuery('(max-width: 767px)');

  const { data: metrics } = useQuery('metrics', getMetrics, {
    refetchInterval: 5000,
  });

  const { data: traffic = [] } = useQuery('traffic', getTraffic, {
    refetchInterval: 5000,
  });

  const handleSelectClient = (port: number) => {
    setSelectedPort(port);
  };

  return (
    <div className="min-h-screen bg-gray-100">
      <Navbar onLogout={onLogout} activeTab={activeTab} onTabChange={setActiveTab} />
      <main className={`max-w-7xl mx-auto py-6 px-4 sm:px-6 lg:px-8 ${isMobile ? 'pb-20' : ''}`}>
        {activeTab === 'dashboard' ? (
          <>
            <div className="grid grid-cols-2 gap-3 sm:gap-5 sm:grid-cols-2 lg:grid-cols-4 mb-6">
              <StatCard
                label="Connected Clients"
                value={String(metrics?.client_count || 0)}
                color="blue"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
                  </svg>
                }
              />
              <StatCard
                label="Active Connections"
                value={String(metrics?.active_connection_count || 0)}
                color="green"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 7h12m0 0l-4-4m4 4l-4 4m0 6H4m0 0l4 4m-4-4l4-4" />
                  </svg>
                }
              />
              <StatCard
                label="Total Bytes In"
                value={formatBytes(metrics?.total_bytes_in || 0)}
                color="purple"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M7 16l-4-4m0 0l4-4m-4 4h18" />
                  </svg>
                }
              />
              <StatCard
                label="Total Bytes Out"
                value={formatBytes(metrics?.total_bytes_out || 0)}
                color="orange"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 8l4 4m0 0l-4 4m4-4H3" />
                  </svg>
                }
              />
            </div>

            <div className="space-y-6">
              <ClientList onSelectClient={handleSelectClient} />
              <TrafficChart traffic={traffic} />
            </div>
          </>
        ) : activeTab === 'quality' ? (
          <QualityPage onSelectClient={handleSelectClient} />
        ) : activeTab === 'shadowsocks' ? (
          <ShadowsocksPage />
        ) : activeTab === 'trojan' ? (
          <TrojanPage />
        ) : (
          <LogsPage />
        )}
      </main>

      {isMobile && <MobileBottomNav activeTab={activeTab} onTabChange={setActiveTab} />}

      {selectedPort !== null && (
        <ClientDetail
          port={selectedPort}
          onClose={() => setSelectedPort(null)}
        />
      )}
    </div>
  );
};
