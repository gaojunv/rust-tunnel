import { lazy } from 'react';
import { createBrowserRouter, RouterProvider, Navigate, Outlet } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { I18nProvider } from './i18n/I18nProvider';
import { ThemeProvider } from './theme/ThemeProvider';
import { PreferencesProvider } from './preferences/PreferencesProvider';
import LoginPage from './pages/LoginPage';
import DashboardPage from './pages/DashboardPage';
import AppLayout from './components/layout/AppLayout';
import { Toaster } from '@/components/ui/sonner';
import './index.css';

// 路由级代码分割：非首页页面按需加载（three.js / xterm / streamdown 等重依赖随之拆出首屏）
const MeshPage = lazy(() => import('./pages/MeshPage'));
const DnsPage = lazy(() => import('./pages/DnsPage'));
const ShadowsocksPage = lazy(() => import('./pages/ShadowsocksPage'));
const TrojanPage = lazy(() => import('./pages/TrojanPage'));
const ReverseProxyPage = lazy(() => import('./pages/ReverseProxyPage'));
const AcmePage = lazy(() => import('./pages/AcmePage'));
const LogsPage = lazy(() => import('./pages/LogsPage'));
const ClientsPage = lazy(() => import('./pages/ClientsPage'));
const ClientDetailPage = lazy(() => import('./pages/ClientDetailPage'));
const SettingsPage = lazy(() => import('./pages/SettingsPage'));
const DownloadsPage = lazy(() => import('./pages/DownloadsPage'));
const LLMPage = lazy(() => import('./pages/LLMPage'));
const KnowledgePage = lazy(() => import('./pages/KnowledgePage'));
const AgentPage = lazy(() => import('./pages/AgentPage'));

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
          { path: '/llm/knowledge', element: <KnowledgePage /> },
          { path: '/llm/kb', element: <Navigate to="/llm/knowledge" replace /> },
          { path: '/agent', element: <AgentPage /> },
          { path: '/agent/memory', element: <Navigate to="/llm/knowledge" replace /> },
          { path: '/downloads', element: <DownloadsPage /> },
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
            <Toaster richColors closeButton position="bottom-right" />
          </QueryClientProvider>
        </I18nProvider>
      </ThemeProvider>
    </PreferencesProvider>
  );
}

export default App;
