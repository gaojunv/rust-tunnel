import { createBrowserRouter, RouterProvider, Navigate, Outlet } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { I18nProvider } from './i18n/I18nProvider';
import { ThemeProvider } from './theme/ThemeProvider';
import { PreferencesProvider } from './preferences/PreferencesProvider';
import LoginPage from './pages/LoginPage';
import DashboardPage from './pages/DashboardPage';
import MeshPage from './pages/MeshPage';
import DnsPage from './pages/DnsPage';
import ShadowsocksPage from './pages/ShadowsocksPage';
import TrojanPage from './pages/TrojanPage';
import ReverseProxyPage from './pages/ReverseProxyPage';
import AcmePage from './pages/AcmePage';
import LogsPage from './pages/LogsPage';
import ClientsPage from './pages/ClientsPage';
import ClientDetailPage from './pages/ClientDetailPage';
import SettingsPage from './pages/SettingsPage';
import LLMPage from './pages/LLMPage';
import KbPage from './pages/KbPage';
import AppLayout from './components/layout/AppLayout';
import './index.css';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5000,
      retry: 1,
    },
  },
});

function ProtectedRoute() {
  const token = localStorage.getItem('auth_token');

  if (!token) {
    return <Navigate to="/login" replace />;
  }

  return <Outlet />;
}

const router = createBrowserRouter([
  {
    path: '/login',
    element: <LoginPage />,
  },
  {
    element: <ProtectedRoute />,
    children: [
      {
        element: <AppLayout />,
        children: [
          { path: '/', element: <Navigate to="/dashboard" replace /> },
          { path: '/dashboard', element: <DashboardPage /> },
          { path: '/mesh', element: <MeshPage /> },
          { path: '/dns', element: <DnsPage /> },
          { path: '/shadowsocks', element: <ShadowsocksPage /> },
          { path: '/trojan', element: <TrojanPage /> },
          { path: '/proxy', element: <ReverseProxyPage /> },
          { path: '/clients', element: <ClientsPage /> },
          { path: '/acme', element: <AcmePage /> },
          { path: '/logs', element: <LogsPage /> },
          { path: '/clients/:name', element: <ClientDetailPage /> },
          { path: '/llm', element: <LLMPage /> },
          { path: '/llm/kb', element: <KbPage /> },
          { path: '/settings', element: <SettingsPage /> },
        ],
      },
    ],
  },
]);

function App() {
  return (
    <PreferencesProvider>
      <ThemeProvider>
        <I18nProvider>
          <QueryClientProvider client={queryClient}>
            <RouterProvider router={router} />
          </QueryClientProvider>
        </I18nProvider>
      </ThemeProvider>
    </PreferencesProvider>
  );
}

export default App;
