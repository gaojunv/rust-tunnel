import { useCallback } from 'react';
import { Outlet, useNavigate } from 'react-router-dom';
import { Header } from './Header';
import { ScrollArea } from '@/components/ui/scroll-area';
import { logout as apiLogout } from '@/api/client';

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
          <div className="container mx-auto p-4 md:p-6">
            <Outlet />
          </div>
        </main>
      </ScrollArea>
    </div>
  );
}
