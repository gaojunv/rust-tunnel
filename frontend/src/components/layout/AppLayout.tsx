import { Suspense, useCallback } from 'react';
import { Outlet, useNavigate } from 'react-router-dom';
import { Header } from './Header';
import { ScrollArea } from '@/components/ui/scroll-area';
import { logout as apiLogout } from '@/api/client';
import { AgentNotificationsProvider } from '@/notifications/NotificationProvider';

export default function AppLayout() {
  const navigate = useNavigate();

  const handleLogout = useCallback(async () => {
    try {
      await apiLogout();
    } catch {
      // Even if the server call fails, clear local state and redirect
    }
    localStorage.removeItem('auth_token');
    navigate('/login');
  }, [navigate]);

  return (
    <div className="flex h-screen flex-col">
      <Header onLogout={handleLogout} />
      <ScrollArea className="flex-1">
        <main>
          <div className="mx-auto w-full max-w-[1400px] px-2 py-3 md:px-6 md:py-6">
            {/* 懒加载页面挂起时仅替换内容区，保留 Header/布局不闪烁 */}
            <Suspense
              fallback={
                <div className="flex h-64 items-center justify-center text-sm text-muted-foreground">
                  Loading…
                </div>
              }
            >
              {/* 全局工作台通知：订阅所有会话事件，标签闪动 + 系统通知（需开启开关） */}
              <AgentNotificationsProvider>
                <Outlet />
              </AgentNotificationsProvider>
            </Suspense>
          </div>
        </main>
      </ScrollArea>
    </div>
  );
}
