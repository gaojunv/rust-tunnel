import { Outlet, useNavigate } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import { cn } from '@/lib/utils';

export default function AppLayout() {
  const navigate = useNavigate();
  const isDesktop = useMediaQuery('(min-width: 768px)');

  const handleLogout = () => {
    localStorage.removeItem('auth_token');
    navigate('/login');
  };

  return (
    <div className="min-h-screen bg-background">
      {isDesktop && <Sidebar onLogout={handleLogout} />}
      <main className={cn('transition-all duration-300', isDesktop ? 'pl-64' : 'pl-0')}>
        <div className="container mx-auto p-4 md:p-6">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
