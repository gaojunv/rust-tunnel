import { lazy, Suspense } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router-dom';
import { cn } from '@/lib/utils';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import { usePreferences } from '@/preferences/PreferencesProvider';
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
  Bot,
  BookOpen,
  Sparkles,
  Download,
} from 'lucide-react';
// three.js 体积大（约 600KB），装饰性背景懒加载，不阻塞首屏
const DataFlowBackground = lazy(() => import('@/components/dataflow/DataFlowBackground'));

interface NavItem {
  labelKey: 'nav.dashboard' | 'nav.clients' | 'nav.mesh' | 'nav.dns' | 'nav.reverseProxy' | 'nav.shadowsocks' | 'nav.trojan' | 'nav.acmeCerts' | 'nav.llmGateway' | 'nav.knowledge' | 'nav.agentWorkbench' | 'nav.logs' | 'nav.clientDownloads' | 'nav.settings';
  icon: React.ReactNode;
  href: string;
}

interface NavGroup {
  labelKey: 'nav.network' | 'nav.proxy' | 'nav.ai' | 'nav.system';
  items: NavItem[];
}

const dashboardItem: NavItem = {
  labelKey: 'nav.dashboard',
  icon: <LayoutDashboard className="h-4 w-4" />,
  href: '/dashboard',
};

const navGroups: NavGroup[] = [
  {
    labelKey: 'nav.network',
    items: [
      { labelKey: 'nav.clients', icon: <Network className="h-4 w-4" />, href: '/clients' },
      { labelKey: 'nav.mesh', icon: <Network className="h-4 w-4" />, href: '/mesh' },
      { labelKey: 'nav.dns', icon: <Globe className="h-4 w-4" />, href: '/dns' },
    ],
  },
  {
    labelKey: 'nav.proxy',
    items: [
      { labelKey: 'nav.reverseProxy', icon: <ArrowLeftRight className="h-4 w-4" />, href: '/proxy' },
      { labelKey: 'nav.shadowsocks', icon: <Shield className="h-4 w-4" />, href: '/shadowsocks' },
      { labelKey: 'nav.trojan', icon: <ShieldCheck className="h-4 w-4" />, href: '/trojan' },
      { labelKey: 'nav.acmeCerts', icon: <FileBadge className="h-4 w-4" />, href: '/acme' },
    ],
  },
  {
    labelKey: 'nav.ai',
    items: [
      { labelKey: 'nav.agentWorkbench', icon: <Sparkles className="h-4 w-4" />, href: '/agent' },
      { labelKey: 'nav.knowledge', icon: <BookOpen className="h-4 w-4" />, href: '/llm/knowledge' },
      { labelKey: 'nav.llmGateway', icon: <Bot className="h-4 w-4" />, href: '/llm' },
    ],
  },
  {
    labelKey: 'nav.system',
    items: [
      { labelKey: 'nav.logs', icon: <ScrollText className="h-4 w-4" />, href: '/logs' },
      { labelKey: 'nav.clientDownloads', icon: <Download className="h-4 w-4" />, href: '/downloads' },
      { labelKey: 'nav.settings', icon: <Settings className="h-4 w-4" />, href: '/settings' },
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

/** 移动端（< md）隐藏页头后，悬浮在右上角的菜单按钮。
 *  独立组件：AppLayout 不再渲染 <header> 时仍需要它挂在布局层（fixed 定位）。
 *  定位基准用 fixed 而非 absolute——滚动页面（ScrollArea 分支）里 absolute 会随内容滚走。 */
export function MobileMenuFab({ onLogout }: HeaderProps) {
  const { t } = useTranslation();
  const location = useLocation();
  const isActive = (href: string) => location.pathname === href;

  return (
    // top 对齐 /agent 顶栏按钮行的垂直中心：main 容器 pt-3(12px) + AgentPage 顶栏
    // p-1.5(6px) = 按钮行从 18px 开始；size=sm 按钮 h-9(36px) 中心在 36px。
    // 本按钮 size=icon h-10(40px)，top = 36 - 20 = 16px(1rem)。刘海屏取安全区更大值；
    // iOS Safari 下由 index.css 覆盖为 0.5rem 贴顶（视口已从刘海下方开始，无需安全区）。
    // ⚠️ fixed 定位不占布局空间：横向占视口右缘 12~52px，页面内容需自行 padding 让位
    //（见 AgentPage 顶栏 pr-[3.25rem]），改 right/尺寸时须同步。
    <div className="mobile-menu-fab fixed right-3 top-[max(env(safe-area-inset-top,0px),1rem)] z-50 md:hidden">
      <MobileNavSheet onLogout={onLogout} t={t} isActive={isActive} />
    </div>
  );
}

export function Header({ onLogout }: HeaderProps) {
  const { t } = useTranslation();
  const location = useLocation();
  const { prefs } = usePreferences();
  // 移动端跳过 three.js（~600KB）装饰背景：省流量/省电，且小屏上几乎不可见。
  // lazy() 组件不渲染就不会触发 chunk 加载，所以条件渲染即可阻止下载。
  // jsdom/SSR 无 matchMedia 时 useMediaQuery 返回 false（移动端行为，不加载）。
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const isActive = (href: string) => location.pathname === href;

  return (
    // 顶部安全区垫高用 env(safe-area-inset-top) 自适应，不写死：
    // cover 视口下 = 状态栏区高度（动态岛机 59px），header 背景延伸覆盖刘海区；
    // contain 视口下 = 0（视口本就从刘海下方开始），不多垫——否则页头上方多出
    // 一段与状态栏等高的空白带。内部 h-14 container 高度不变。
    <header className="sticky top-0 z-40 border-b border-border/70 bg-card/60 backdrop-blur-xl shadow-[inset_0_1px_0_0_hsl(var(--foreground)/0.04),0_10px_30px_-12px_hsl(var(--primary)/0.22),0_2px_8px_-4px_hsl(var(--foreground)/0.08)] pt-[env(safe-area-inset-top,0px)]">
      {/* 装饰层（数据流光效 + 底部流光渐变线）。
          overflow-hidden 只加在装饰层上：若加在 header 上会把 ThemeToggle
          弹出到 header 外的下拉菜单一起裁掉，导致主题切换无法点击。
          titleEffect === 'none' 或移动端时跳过数据流背景（WebGL），保留底线渐变。 */}
      {prefs.titleEffect !== 'none' && isDesktop && (
        <Suspense fallback={null}>
          <DataFlowBackground />
        </Suspense>
      )}
      <div
        aria-hidden
        className="header-light-flow pointer-events-none absolute inset-x-0 bottom-0 h-[2px] opacity-70"
      />
      <div className="container relative mx-auto flex h-14 items-center gap-2 px-4 md:px-6">
        <Link to="/" className="flex items-center gap-2 font-semibold">
          <Logo className="logo-glow-breathe h-7 w-7 rounded-lg shadow-glow" />
          <span className="text-aurora hidden font-semibold sm:inline">
            Aurora Tunnel
          </span>
        </Link>

        {/* Desktop navigation */}
        <nav className="ml-4 hidden items-center gap-1 md:flex">
          <Link to={dashboardItem.href} className={navLinkClass(isActive(dashboardItem.href))}>
            {dashboardItem.icon}
            <span>{t(dashboardItem.labelKey)}</span>
          </Link>
          {navGroups.map((group) => {
            const groupActive = group.items.some((item) => isActive(item.href));
            return (
              <DropdownMenu key={group.labelKey}>
                <DropdownMenuTrigger
                  className={cn(navLinkClass(groupActive), 'outline-none')}
                  aria-label={t(group.labelKey)}
                >
                  <span>{t(group.labelKey)}</span>
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
                        <span>{t(item.labelKey)}</span>
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
          aria-label={t('nav.logout')}
          className="hidden text-muted-foreground hover:text-destructive md:inline-flex"
        >
          <LogOut className="h-4 w-4" />
        </Button>

        {/* Mobile navigation（页头内副本；md 以下整页头隐藏时由 MobileMenuFab 接管） */}
        <div className="md:hidden">
          <MobileNavSheet onLogout={onLogout} t={t} isActive={isActive} />
        </div>
      </div>
    </header>
  );
}

/** 移动端侧边抽屉导航（Sheet），页头内按钮与 MobileMenuFab 共用。 */
function MobileNavSheet({
  onLogout,
  t,
  isActive,
}: {
  onLogout: () => void;
  t: (key: NavItem['labelKey'] | NavGroup['labelKey'] | 'nav.openMenu' | 'nav.logout') => string;
  isActive: (href: string) => boolean;
}) {
  return (
    <Sheet>
      <SheetTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={t('nav.openMenu')}>
          <Menu className="h-5 w-5" />
        </Button>
      </SheetTrigger>
      <SheetContent side="right" className="flex w-72 flex-col p-0">
        <SheetHeader className="border-b p-4">
          <SheetTitle className="flex items-center gap-2 text-left">
            <Logo className="logo-glow-breathe h-7 w-7 rounded-lg shadow-glow" />
            <span className="text-aurora">Aurora Tunnel</span>
          </SheetTitle>
        </SheetHeader>
        <nav className="flex-1 space-y-4 overflow-y-auto p-4">
          <div className="space-y-1">
            <SheetClose asChild>
              <Link to={dashboardItem.href} className={navLinkClass(isActive(dashboardItem.href))}>
                {dashboardItem.icon}
                <span>{t(dashboardItem.labelKey)}</span>
              </Link>
            </SheetClose>
          </div>
          {navGroups.map((group) => (
            <div key={group.labelKey} className="space-y-1">
              <p className="px-3 text-xs font-medium uppercase tracking-wider text-muted-foreground">
                {t(group.labelKey)}
              </p>
              {group.items.map((item) => (
                <SheetClose asChild key={item.href}>
                  <Link to={item.href} className={navLinkClass(isActive(item.href))}>
                    {item.icon}
                    <span>{t(item.labelKey)}</span>
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
            {t('nav.logout')}
          </Button>
        </div>
      </SheetContent>
    </Sheet>
  );
}
