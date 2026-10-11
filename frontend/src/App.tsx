import { lazy } from 'react';
import { createBrowserRouter, RouterProvider, Navigate, Outlet } from 'react-router-dom';
import { MutationCache, QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { toast } from 'sonner';
import { I18nProvider } from './i18n/I18nProvider';
import i18n from './i18n';
import { getApiErrorMessage } from './api/client';
import { ThemeProvider } from './theme/ThemeProvider';
import { PreferencesProvider } from './preferences/PreferencesProvider';
import LoginPage from './pages/LoginPage';
import DashboardPage from './pages/DashboardPage';
import AppLayout from './components/layout/AppLayout';
import ErrorPage from './pages/ErrorPage';
import NotFoundPage from './pages/NotFoundPage';
import { Toaster } from '@/components/ui/sonner';
import './index.css';

// 路由级代码分割：非首页页面按需加载（three.js / xterm / streamdown 等重依赖随之拆出首屏）
const MeshPage = lazy(() => import('./pages/MeshPage'));
const DnsPage = lazy(() => import('./pages/DnsPage'));
const ShadowsocksPage = lazy(() => import('./pages/ShadowsocksPage'));
const TrojanPage = lazy(() => import('./pages/TrojanPage'));
const RustdeskPage = lazy(() => import('./pages/RustdeskPage'));
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

// 全局 mutation 反馈：失败默认错误 toast（meta.muteErrorToast 豁免——错误已内联展示、
// 乐观回滚自解释等场景），成功按 meta.successToast（i18n key）toast。
// 本地 onError/onSuccess 仍照常触发（全局回调是追加而非替代）。
const queryClient = new QueryClient({
  mutationCache: new MutationCache({
    onError: (error, _variables, _context, mutation) => {
      if (mutation.options.meta?.muteErrorToast) return;
      // 401 由 axios 拦截器清 token 并硬跳转登录页，错误 toast 只会闪现干扰
      if ((error as { response?: { status?: number } } | null)?.response?.status === 401) return;
      toast.error(i18n.t('common.toast.actionFailed', { error: getApiErrorMessage(error) }));
    },
    onSuccess: (_data, _variables, _context, mutation) => {
      const key = mutation.options.meta?.successToast;
      // meta 为运行时字符串 key，需宽签名 t（同 useApprovalMutation 的 needsUpgradeKey 处理）
      if (key) toast.success((i18n.t as (k: string) => string)(key));
    },
  }),
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
        errorElement: <ErrorPage />,
        children: [
          { path: '/', element: <Navigate to="/dashboard" replace /> },
          { path: '/dashboard', element: <DashboardPage /> },
          { path: '/mesh', element: <MeshPage /> },
          { path: '/dns', element: <DnsPage /> },
          { path: '/shadowsocks', element: <ShadowsocksPage /> },
          { path: '/trojan', element: <TrojanPage /> },
          { path: '/rustdesk', element: <RustdeskPage /> },
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
          { path: '*', element: <NotFoundPage /> },
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
