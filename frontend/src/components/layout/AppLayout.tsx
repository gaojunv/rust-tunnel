import { Suspense, useCallback } from 'react';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import { Header, MobileMenuFab } from './Header';
import { ScrollArea } from '@/components/ui/scroll-area';
import { cn } from '@/lib/utils';
import { logout as apiLogout } from '@/api/client';
import { AgentNotificationsProvider } from '@/notifications/NotificationProvider';
import { useMediaQuery } from '@/hooks/useMediaQuery';

export default function AppLayout() {
  const navigate = useNavigate();
  const location = useLocation();
  // 移动端（< md）不渲染页头：菜单按钮由 MobileMenuFab 悬浮在右上角，
  // 腾出整段页头高度给内容。SSR/jsdom 无 matchMedia 时返回 false（按移动端处理）。
  const isDesktop = useMediaQuery('(min-width: 768px)');

  // AI 工作台是唯一「整页自管理滚动」的路由：消息区/面板在页面内部各自滚动，
  // 外层再包一个整页滚动容器会叠出双重滚动条（历史：5ad703a 修过一次仍复发）。
  // 对它走非滚动分支：外层 h-dvh 动态视口高度，AgentPage 用 h-full 精确填满
  // Header 以下空间，外层永不产生滚动条。
  // （iOS 键盘三问题由 index.html 的 contain 视口根治，与高度单位无关；
  //   曾改用 --vh(innerHeight) 反而因 iOS 取值不准导致页头空白，已回退 dvh。）
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
    <div className={cn('flex flex-col', isAgentRoute ? 'h-dvh' : 'h-screen')}>
      {isDesktop ? <Header onLogout={handleLogout} /> : <MobileMenuFab onLogout={handleLogout} />}
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
