import { useState, useCallback } from 'react';
import { Outlet, useNavigate } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { MobileNav } from './MobileNav';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import { cn } from '@/lib/utils';
import { logout as apiLogout } from '@/api/client';

export default function AppLayout() {
  const navigate = useNavigate();
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    return localStorage.getItem('sidebar-collapsed') === 'true';
  });

  const handleCollapseChange = useCallback((collapsed: boolean) => {
    setSidebarCollapsed(collapsed);
  }, []);

  const handleLogout = async () => {
    try {
      await apiLogout();
    } catch {
      // Even if the server call fails, clear local state and redirect
    }
    localStorage.removeItem('auth_token');
    navigate('/login');
  };

  return (
    <div className="min-h-screen">
      {isDesktop && <Sidebar onLogout={handleLogout} onCollapseChange={handleCollapseChange} />}
      <main
        className={cn(
          'transition-all duration-300',
          isDesktop ? (sidebarCollapsed ? 'pl-16' : 'pl-64') : 'pb-16'
        )}
      >
        <div className="container mx-auto p-4 md:p-6">
          <Outlet />
        </div>
      </main>
      {!isDesktop && <MobileNav onLogout={handleLogout} />}
    </div>
  );
}
