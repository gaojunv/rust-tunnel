import { Link, useLocation } from 'react-router-dom';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet';
import { Separator } from '@/components/ui/separator';
import { ThemeToggle } from '@/components/shared/ThemeToggle';
import { Logo } from '@/components/shared/Logo';
import {
  LayoutDashboard,
  Signal,
  Network,
  Globe,
  Shield,
  ShieldCheck,
  FileBadge,
  ArrowLeftRight,
  ScrollText,
  Settings,
  ChevronDown,
  Menu,
  LogOut,
} from 'lucide-react';

interface NavItem {
  label: string;
  icon: React.ReactNode;
  href: string;
}

interface NavGroup {
  label: string;
  items: NavItem[];
}

const dashboardItem: NavItem = {
  label: 'Dashboard',
  icon: <LayoutDashboard className="h-4 w-4" />,
  href: '/dashboard',
};

const navGroups: NavGroup[] = [
  {
    label: 'Network',
    items: [
      { label: 'Quality', icon: <Signal className="h-4 w-4" />, href: '/quality' },
      { label: 'Mesh', icon: <Network className="h-4 w-4" />, href: '/mesh' },
      { label: 'DNS', icon: <Globe className="h-4 w-4" />, href: '/dns' },
    ],
  },
  {
    label: 'Proxy',
    items: [
      { label: 'Reverse Proxy', icon: <ArrowLeftRight className="h-4 w-4" />, href: '/proxy' },
      { label: 'Shadowsocks', icon: <Shield className="h-4 w-4" />, href: '/shadowsocks' },
      { label: 'Trojan', icon: <ShieldCheck className="h-4 w-4" />, href: '/trojan' },
      { label: 'ACME Certs', icon: <FileBadge className="h-4 w-4" />, href: '/acme' },
    ],
  },
  {
    label: 'System',
    items: [
      { label: 'Logs', icon: <ScrollText className="h-4 w-4" />, href: '/logs' },
      { label: 'Settings', icon: <Settings className="h-4 w-4" />, href: '/settings' },
    ],
  },
];

const navLinkClass = (active: boolean) =>
  cn(
    'flex items-center gap-1.5 rounded-lg px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground',
    active ? 'bg-primary/10 text-primary' : 'text-muted-foreground'
  );

interface HeaderProps {
  onLogout: () => void;
}

export function Header({ onLogout }: HeaderProps) {
  const location = useLocation();
  const isActive = (href: string) => location.pathname === href;

  return (
    <header className="sticky top-0 z-40 border-b bg-card/60 backdrop-blur-xl">
      {/* 光影流动装饰层（环境辉光 + 扫过高光 + 底部流光渐变线）。
          overflow-hidden 只加在装饰层上：若加在 header 上会把 ThemeToggle
          弹出到 header 外的下拉菜单一起裁掉，导致主题切换无法点击 */}
      <div aria-hidden className="pointer-events-none absolute inset-0 overflow-hidden">
        <div className="header-ambient-glow absolute inset-0" />
        <div className="header-sheen absolute inset-0" />
      </div>
      <div
        aria-hidden
        className="header-light-flow pointer-events-none absolute inset-x-0 bottom-0 h-[2px] opacity-70"
      />
      <div className="container relative mx-auto flex h-14 items-center gap-2 px-4 md:px-6">
        <Link to="/" className="flex items-center gap-2 font-semibold">
          <Logo className="h-7 w-7 rounded-lg shadow-glow" />
          <span className="hidden bg-gradient-to-r from-foreground to-muted-foreground bg-clip-text text-transparent sm:inline">
            Rust Tunnel
          </span>
        </Link>

        {/* Desktop navigation */}
        <nav className="ml-4 hidden items-center gap-1 md:flex">
          <Link to={dashboardItem.href} className={navLinkClass(isActive(dashboardItem.href))}>
            {dashboardItem.icon}
            <span>{dashboardItem.label}</span>
          </Link>
          {navGroups.map((group) => {
            const groupActive = group.items.some((item) => isActive(item.href));
            return (
              <DropdownMenu key={group.label}>
                <DropdownMenuTrigger
                  className={cn(navLinkClass(groupActive), 'outline-none')}
                  aria-label={`${group.label} menu`}
                >
                  <span>{group.label}</span>
                  <ChevronDown className="h-3.5 w-3.5 opacity-60" />
                </DropdownMenuTrigger>
                <DropdownMenuContent align="start">
                  {group.items.map((item) => (
                    <DropdownMenuItem
                      key={item.href}
                      asChild
                      className={cn(isActive(item.href) && 'bg-primary/10 text-primary')}
                    >
                      <Link to={item.href}>
                        {item.icon}
                        <span>{item.label}</span>
                      </Link>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              </DropdownMenu>
            );
          })}
        </nav>

        <div className="flex-1" />

        <ThemeToggle />
        <Button
          variant="ghost"
          size="icon"
          onClick={onLogout}
          aria-label="Logout"
          className="hidden text-muted-foreground hover:text-destructive md:inline-flex"
        >
          <LogOut className="h-4 w-4" />
        </Button>

        {/* Mobile navigation */}
        <Sheet>
          <SheetTrigger asChild>
            <Button variant="ghost" size="icon" aria-label="Open menu" className="md:hidden">
              <Menu className="h-5 w-5" />
            </Button>
          </SheetTrigger>
          <SheetContent side="right" className="flex w-72 flex-col p-0">
            <SheetHeader className="border-b p-4">
              <SheetTitle className="flex items-center gap-2 text-left">
                <Logo className="h-7 w-7 rounded-lg shadow-glow" />
                Rust Tunnel
              </SheetTitle>
            </SheetHeader>
            <nav className="flex-1 space-y-4 overflow-y-auto p-4">
              <div className="space-y-1">
                <SheetClose asChild>
                  <Link to={dashboardItem.href} className={navLinkClass(isActive(dashboardItem.href))}>
                    {dashboardItem.icon}
                    <span>{dashboardItem.label}</span>
                  </Link>
                </SheetClose>
              </div>
              {navGroups.map((group) => (
                <div key={group.label} className="space-y-1">
                  <p className="px-3 text-xs font-medium uppercase tracking-wider text-muted-foreground">
                    {group.label}
                  </p>
                  {group.items.map((item) => (
                    <SheetClose asChild key={item.href}>
                      <Link to={item.href} className={navLinkClass(isActive(item.href))}>
                        {item.icon}
                        <span>{item.label}</span>
                      </Link>
                    </SheetClose>
                  ))}
                </div>
              ))}
            </nav>
            <Separator />
            <div className="p-4">
              <Button
                variant="ghost"
                className="w-full justify-start text-destructive hover:text-destructive"
                onClick={onLogout}
              >
                <LogOut className="mr-2 h-4 w-4" />
                Logout
              </Button>
            </div>
          </SheetContent>
        </Sheet>
      </div>
    </header>
  );
}
