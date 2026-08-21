import { Suspense, useCallback } from 'react';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import { Header } from './Header';
import { ScrollArea } from '@/components/ui/scroll-area';
import { cn } from '@/lib/utils';
import { logout as apiLogout } from '@/api/client';
import { AgentNotificationsProvider } from '@/notifications/NotificationProvider';

export default function AppLayout() {
  const navigate = useNavigate();
  const location = useLocation();

  // AI 工作台是唯一「整页自管理滚动」的路由：消息区/面板在页面内部各自滚动，
  // 外层再包一个整页滚动容器会叠出双重滚动条（历史：5ad703a 修过一次仍复发）。
  // 对它走非滚动分支：外层 h-vvh（= window.innerHeight，对标 Kimi 的 --vh 方案）
  // 替代 h-dvh——innerHeight 在 iOS PWA 键盘弹收时跟随可视视口，不像 dvh 会触发
  // 布局视口压缩导致整页内容上移。AgentPage 用 h-full 精确填满 Header 以下空间，
  // 外层永不产生滚动条。
  const isAgentRoute = location.pathname === '/agent';

  const handleLogout = useCallback(async () => {
    try {
      await apiLogout();
    } catch {
      // Even if the server call fails, clear local state and redirect
    }
    localStorage.removeItem('auth_token');
    navigate('/login');
  }, [navigate]);

  // 懒加载页面挂起时仅替换内容区，保留 Header/布局不闪烁
  const page = (
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
  );

  return (
    <div className={cn('flex flex-col', isAgentRoute ? 'h-vvh' : 'h-screen')}>
      <Header onLogout={handleLogout} />
      {isAgentRoute ? (
        <main className="min-h-0 flex-1 overflow-hidden">
          <div className="mx-auto h-full w-full max-w-[1400px] overflow-hidden px-2 py-3 md:px-6 md:py-6">
            {page}
          </div>
        </main>
      ) : (
        <ScrollArea className="flex-1">
          <main>
            <div className="mx-auto w-full max-w-[1400px] px-2 py-3 md:px-6 md:py-6">
              {page}
            </div>
          </main>
        </ScrollArea>
      )}
    </div>
  );
}
