import { Link, useLocation } from 'react-router-dom';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import {
  LayoutDashboard,
  Signal,
  Network,
  Globe,
  Shield,
  ShieldCheck,
  FileText,
  Menu,
  LogOut,
  Settings,
} from 'lucide-react';

const coreTabs = [
  { label: 'Dashboard', icon: <LayoutDashboard className="h-5 w-5" />, href: '/dashboard' },
  { label: 'Quality', icon: <Signal className="h-5 w-5" />, href: '/quality' },
  { label: 'Mesh', icon: <Network className="h-5 w-5" />, href: '/mesh' },
  { label: 'DNS', icon: <Globe className="h-5 w-5" />, href: '/dns' },
  { label: 'Shadowsocks', icon: <ShieldCheck className="h-5 w-5" />, href: '/shadowsocks' },
];

const moreItems = [
  { label: 'Trojan', icon: <Shield className="h-5 w-5" />, href: '/trojan' },
  { label: 'Logs', icon: <FileText className="h-5 w-5" />, href: '/logs' },
  { label: 'Settings', icon: <Settings className="h-5 w-5" />, href: '/settings' },
];

interface MobileNavProps {
  onLogout: () => void;
}

export function MobileNav({ onLogout }: MobileNavProps) {
  const location = useLocation();

  return (
    <div className="fixed bottom-0 left-0 right-0 z-40 border-t bg-card md:hidden">
      <nav className="flex h-16 items-center justify-around">
        {coreTabs.map((tab) => (
          <Link
            key={tab.href}
            to={tab.href}
            className={cn(
              'flex flex-col items-center gap-1 px-3 py-2 text-xs font-medium',
              location.pathname === tab.href
                ? 'text-primary'
                : 'text-muted-foreground'
            )}
          >
            {tab.icon}
            <span>{tab.label}</span>
          </Link>
        ))}

        <Sheet>
          <SheetTrigger asChild>
            <button
              className={cn(
                'flex flex-col items-center gap-1 px-3 py-2 text-xs font-medium',
                moreItems.some((item) => item.href === location.pathname)
                  ? 'text-primary'
                  : 'text-muted-foreground'
              )}
            >
              <Menu className="h-5 w-5" />
              <span>More</span>
            </button>
          </SheetTrigger>
          <SheetContent side="bottom" className="h-[50vh]">
            <nav className="grid gap-2 py-4">
              {moreItems.map((item) => (
                <Link
                  key={item.href}
                  to={item.href}
                  className={cn(
                    'flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium hover:bg-accent',
                    location.pathname === item.href
                      ? 'bg-accent text-accent-foreground'
                      : 'text-muted-foreground'
                  )}
                >
                  {item.icon}
                  <span>{item.label}</span>
                </Link>
              ))}
              <Button
                variant="ghost"
                className="justify-start text-destructive hover:text-destructive"
                onClick={onLogout}
              >
                <LogOut className="mr-3 h-5 w-5" />
                <span>Logout</span>
              </Button>
            </nav>
          </SheetContent>
        </Sheet>
      </nav>
    </div>
  );
}
