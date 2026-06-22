import { useMutation } from 'react-query';
import { logout } from '../api/client';
import { ThemeToggle } from './shared/ThemeToggle';

interface NavbarProps {
  onLogout: () => void;
  activeTab: 'dashboard' | 'mesh' | 'dns' | 'quality' | 'shadowsocks' | 'trojan' | 'logs';
  onTabChange: (tab: 'dashboard' | 'mesh' | 'dns' | 'quality' | 'shadowsocks' | 'trojan' | 'logs') => void;
}

export const Navbar = ({ onLogout, activeTab, onTabChange }: NavbarProps) => {
  const logoutMutation = useMutation(logout, {
    onSuccess: () => {
      onLogout();
    },
  });

  const handleLogout = () => {
    logoutMutation.mutate();
  };

  return (
    <nav className="bg-gray-800 dark:bg-slate-900 dark:border-b dark:border-slate-800">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex items-center justify-between h-16">
          <div className="flex items-center">
            <div className="flex-shrink-0">
              <h1 className="text-white text-xl font-bold">Rust Tunnel</h1>
            </div>
            <div className="hidden md:flex ml-10 space-x-4">
              <button
                onClick={() => onTabChange('dashboard')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'dashboard'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Dashboard
              </button>
              <button
                onClick={() => onTabChange('mesh')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'mesh'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Mesh
              </button>
              <button
                onClick={() => onTabChange('dns')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'dns'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                DNS
              </button>
              <button
                onClick={() => onTabChange('quality')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'quality'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Quality
              </button>
              <button
                onClick={() => onTabChange('shadowsocks')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'shadowsocks'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Shadowsocks
              </button>
              <button
                onClick={() => onTabChange('trojan')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'trojan'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Trojan
              </button>
              <button
                onClick={() => onTabChange('logs')}
                className={`px-3 py-2 rounded-md text-sm font-medium ${
                  activeTab === 'logs'
                    ? 'bg-gray-900 text-white dark:bg-slate-800'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
                }`}
              >
                Logs
              </button>
            </div>
          </div>
          <div className="ml-4 flex items-center md:ml-6 gap-3">
            <ThemeToggle />
            <button
              onClick={handleLogout}
              disabled={logoutMutation.isLoading}
              className="bg-gray-700 hover:bg-gray-600 text-white px-4 py-2 rounded-md text-sm font-medium disabled:opacity-50 dark:bg-slate-700 dark:hover:bg-slate-600"
            >
              Logout
            </button>
          </div>
        </div>
      </div>
    </nav>
  );
};
