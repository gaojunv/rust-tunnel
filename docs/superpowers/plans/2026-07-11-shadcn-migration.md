# shadcn/ui Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate rust-tunnel frontend from hand-built Tailwind components to shadcn/ui with sidebar-based dashboard layout, React Query v5, and React Router v6.

**Architecture:** Incremental migration in 4 phases: Infrastructure → Layout → Core Pages → Remaining Pages. Each phase builds on the previous, maintaining a working application throughout.

**Tech Stack:** React 18, shadcn/ui, Tailwind CSS, React Query v5 (TanStack Query), React Router v6, Recharts, Axios

---

## File Structure Overview

**Files to Create:**
- `frontend/src/components/ui/` - shadcn components (auto-generated)
- `frontend/src/components/layout/AppLayout.tsx`
- `frontend/src/components/layout/Sidebar.tsx`
- `frontend/src/components/layout/MobileNav.tsx`
- `frontend/src/components/layout/PageHeader.tsx`
- `frontend/src/components/shared/StatCard.tsx` (rewrite)
- `frontend/src/components/shared/ChartContainer.tsx` (rewrite)
- `frontend/src/components/shared/QualityBadge.tsx`
- `frontend/src/pages/LoginPage.tsx`
- `frontend/src/pages/DashboardPage.tsx`
- `frontend/src/pages/QualityPage.tsx`
- `frontend/src/pages/MeshPage.tsx`
- `frontend/src/pages/DnsPage.tsx`
- `frontend/src/pages/ShadowsocksPage.tsx`
- `frontend/src/pages/TrojanPage.tsx`
- `frontend/src/pages/LogsPage.tsx`
- `frontend/src/pages/ClientDetailPage.tsx`
- `frontend/src/lib/utils.ts`

**Files to Modify:**
- `frontend/package.json` - Add dependencies
- `frontend/tailwind.config.js` - Add tailwindcss-animate
- `frontend/tsconfig.json` - Add path alias
- `frontend/src/App.tsx` - Router setup
- `frontend/src/main.tsx` - Router provider
- `frontend/src/index.css` - shadcn CSS variables
- `frontend/src/api/client.ts` - React Query v5 hooks

**Files to Delete (after migration):**
- `frontend/src/components/Navbar.tsx`
- `frontend/src/components/Dashboard.tsx`
- `frontend/src/components/Login.tsx`
- `frontend/src/components/shared/MobileBottomNav.tsx`

---

## Phase 1: Infrastructure

### Task 1.1: Install shadcn/ui and Configure Tailwind

**Files:**
- Modify: `frontend/package.json`
- Modify: `frontend/tailwind.config.js`
- Modify: `frontend/tsconfig.json`
- Create: `frontend/src/lib/utils.ts`
- Create: `frontend/src/index.css` (update)

- [ ] **Step 1: Initialize shadcn/ui**

```bash
cd frontend
npx shadcn@latest init
```

Select:
- Style: Default
- Base color: Zinc
- CSS variables: Yes

- [ ] **Step 2: Add tailwindcss-animate plugin**

```bash
npm install tailwindcss-animate
```

Update `frontend/tailwind.config.js`:

```javascript
/** @type {import('tailwindcss').Config} */
export default {
  darkMode: ["class"],
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        border: "hsl(var(--border))",
        input: "hsl(var(--input))",
        ring: "hsl(var(--ring))",
        background: "hsl(var(--background))",
        foreground: "hsl(var(--foreground))",
        primary: {
          DEFAULT: "hsl(var(--primary))",
          foreground: "hsl(var(--primary-foreground))",
        },
        secondary: {
          DEFAULT: "hsl(var(--secondary))",
          foreground: "hsl(var(--secondary-foreground))",
        },
        destructive: {
          DEFAULT: "hsl(var(--destructive))",
          foreground: "hsl(var(--destructive-foreground))",
        },
        muted: {
          DEFAULT: "hsl(var(--muted))",
          foreground: "hsl(var(--muted-foreground))",
        },
        accent: {
          DEFAULT: "hsl(var(--accent))",
          foreground: "hsl(var(--accent-foreground))",
        },
        popover: {
          DEFAULT: "hsl(var(--popover))",
          foreground: "hsl(var(--popover-foreground))",
        },
        card: {
          DEFAULT: "hsl(var(--card))",
          foreground: "hsl(var(--card-foreground))",
        },
      },
      borderRadius: {
        lg: "var(--radius)",
        md: "calc(var(--radius) - 2px)",
        sm: "calc(var(--radius) - 4px)",
      },
      keyframes: {
        "accordion-down": {
          from: { height: "0" },
          to: { height: "var(--radix-accordion-content-height)" },
        },
        "accordion-up": {
          from: { height: "var(--radix-accordion-content-height)" },
          to: { height: "0" },
        },
      },
      animation: {
        "accordion-down": "accordion-down 0.2s ease-out",
        "accordion-up": "accordion-up 0.2s ease-out",
      },
    },
  },
  plugins: [require("tailwindcss-animate")],
}
```

- [ ] **Step 3: Add path alias to tsconfig.json**

Update `frontend/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "useDefineForClassFields": true,
    "lib": ["ES2020", "DOM", "DOM.Iterable"],
    "module": "ESNext",
    "skipLibCheck": true,
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "resolveJsonModule": true,
    "isolatedModules": true,
    "noEmit": true,
    "jsx": "react-jsx",
    "strict": true,
    "noUnusedLocals": true,
    "noUnusedParameters": true,
    "noFallthroughCasesInSwitch": true,
    "baseUrl": ".",
    "paths": {
      "@/*": ["./src/*"]
    }
  },
  "include": ["src"],
  "references": [{ "path": "./tsconfig.node.json" }]
}
```

- [ ] **Step 4: Create cn utility**

Create `frontend/src/lib/utils.ts`:

```typescript
import { type ClassValue, clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}
```

- [ ] **Step 5: Install required shadcn components**

```bash
cd frontend
npx shadcn@latest add button card input table badge sheet collapsible separator dropdown-menu tooltip skeleton tabs select switch scroll-area
```

- [ ] **Step 6: Verify shadcn components installed**

```bash
ls frontend/src/components/ui/
```

Expected: Should see `button.tsx`, `card.tsx`, `input.tsx`, etc.

- [ ] **Step 7: Commit**

```bash
git add frontend/
git commit -m "feat(frontend): initialize shadcn/ui with Tailwind config"
```

---

### Task 1.2: Upgrade React Query v3 to v5

**Files:**
- Modify: `frontend/package.json`
- Modify: `frontend/src/App.tsx`
- Modify: `frontend/src/api/client.ts`
- Modify: All component files using React Query

- [ ] **Step 1: Install TanStack React Query v5**

```bash
cd frontend
npm uninstall react-query
npm install @tanstack/react-query
```

- [ ] **Step 2: Update QueryClientProvider in App.tsx**

Update imports in `frontend/src/App.tsx`:

```typescript
import { useState, useEffect } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { Login } from './components/Login';
import { Dashboard } from './components/Dashboard';
import { checkHealth } from './api/client';
import { ThemeProvider } from './theme/ThemeProvider';
import './index.css';
```

- [ ] **Step 3: Create React Query v5 hooks in api/client.ts**

Create new hooks file `frontend/src/api/hooks.ts`:

```typescript
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from './client';

// Health check
export function useHealth() {
  return useQuery({
    queryKey: ['health'],
    queryFn: () => api.get('/api/health').then(res => res.data),
  });
}

// Clients
export function useClients() {
  return useQuery({
    queryKey: ['clients'],
    queryFn: () => api.get('/api/clients').then(res => res.data),
    refetchInterval: 5000,
  });
}

// Metrics
export function useMetrics() {
  return useQuery({
    queryKey: ['metrics'],
    queryFn: () => api.get('/api/metrics').then(res => res.data),
    refetchInterval: 5000,
  });
}

// Traffic
export function useTraffic(port?: number, hours = 24) {
  return useQuery({
    queryKey: ['traffic', port, hours],
    queryFn: () => api.get(`/api/traffic?port=${port}&hours=${hours}`).then(res => res.data),
    enabled: port !== undefined,
  });
}

// Quality
export function useQuality(port?: number) {
  return useQuery({
    queryKey: ['quality', port],
    queryFn: () => api.get(`/api/quality/${port}`).then(res => res.data),
    enabled: port !== undefined,
    refetchInterval: 10000,
  });
}

export function useQualitySummary() {
  return useQuery({
    queryKey: ['quality-summary'],
    queryFn: () => api.get('/api/quality/summary').then(res => res.data),
    refetchInterval: 10000,
  });
}

// Shadowsocks
export function useShadowsocksConfig() {
  return useQuery({
    queryKey: ['shadowsocks-config'],
    queryFn: () => api.get('/api/shadowsocks/config').then(res => res.data),
  });
}

export function useUpdateShadowsocksConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: unknown) => api.post('/api/shadowsocks/config', config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['shadowsocks-config'] });
    },
  });
}

// Trojan
export function useTrojanConfig() {
  return useQuery({
    queryKey: ['trojan-config'],
    queryFn: () => api.get('/api/trojan/config').then(res => res.data),
  });
}

export function useUpdateTrojanConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: unknown) => api.post('/api/trojan/config', config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['trojan-config'] });
    },
  });
}

// Logs
export function useLogs(page = 1, limit = 50, level?: string) {
  return useQuery({
    queryKey: ['logs', page, limit, level],
    queryFn: () => api.get(`/api/logs?page=${page}&limit=${limit}&level=${level}`).then(res => res.data),
  });
}

// Login
export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => api.post('/api/login', { password }).then(res => res.data),
  });
}

export function useLogout() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => api.post('/api/logout'),
    onSuccess: () => {
      queryClient.clear();
      localStorage.removeItem('auth_token');
    },
  });
}
```

- [ ] **Step 4: Verify build succeeds**

```bash
cd frontend
npm run build
```

Expected: Build succeeds (components will still use old imports, but types should be compatible)

- [ ] **Step 5: Commit**

```bash
git add frontend/
git commit -m "feat(frontend): upgrade React Query v3 to v5 (TanStack Query)"
```

---

### Task 1.3: Add React Router v6

**Files:**
- Modify: `frontend/package.json`
- Modify: `frontend/src/App.tsx`
- Modify: `frontend/src/main.tsx`

- [ ] **Step 1: Install React Router**

```bash
cd frontend
npm install react-router-dom
npm install -D @types/react-router-dom
```

- [ ] **Step 2: Create router configuration**

Rewrite `frontend/src/App.tsx`:

```typescript
import { createBrowserRouter, RouterProvider, Navigate, Outlet } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { ThemeProvider } from './theme/ThemeProvider';
import LoginPage from './pages/LoginPage';
import DashboardPage from './pages/DashboardPage';
import QualityPage from './pages/QualityPage';
import MeshPage from './pages/MeshPage';
import DnsPage from './pages/DnsPage';
import ShadowsocksPage from './pages/ShadowsocksPage';
import TrojanPage from './pages/TrojanPage';
import LogsPage from './pages/LogsPage';
import ClientDetailPage from './pages/ClientDetailPage';
import AppLayout from './components/layout/AppLayout';
import './index.css';

const queryClient = new QueryClient({
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
        children: [
          { path: '/', element: <Navigate to="/dashboard" replace /> },
          { path: '/dashboard', element: <DashboardPage /> },
          { path: '/quality', element: <QualityPage /> },
          { path: '/mesh', element: <MeshPage /> },
          { path: '/dns', element: <DnsPage /> },
          { path: '/shadowsocks', element: <ShadowsocksPage /> },
          { path: '/trojan', element: <TrojanPage /> },
          { path: '/logs', element: <LogsPage /> },
          { path: '/clients/:port', element: <ClientDetailPage /> },
        ],
      },
    ],
  },
]);

function App() {
  return (
    <ThemeProvider>
      <QueryClientProvider client={queryClient}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </ThemeProvider>
  );
}

export default App;
```

- [ ] **Step 3: Create placeholder page components**

Create `frontend/src/pages/LoginPage.tsx`:

```typescript
export default function LoginPage() {
  return <div>Login Page - To be implemented</div>;
}
```

Create `frontend/src/pages/DashboardPage.tsx`:

```typescript
export default function DashboardPage() {
  return <div>Dashboard Page - To be implemented</div>;
}
```

Create similar placeholders for: `QualityPage.tsx`, `MeshPage.tsx`, `DnsPage.tsx`, `ShadowsocksPage.tsx`, `TrojanPage.tsx`, `LogsPage.tsx`, `ClientDetailPage.tsx`

- [ ] **Step 4: Create AppLayout placeholder**

Create `frontend/src/components/layout/AppLayout.tsx`:

```typescript
import { Outlet } from 'react-router-dom';

export default function AppLayout() {
  return (
    <div className="min-h-screen bg-background">
      <Outlet />
    </div>
  );
}
```

- [ ] **Step 5: Verify build succeeds**

```bash
cd frontend
npm run build
```

Expected: Build succeeds with placeholder pages

- [ ] **Step 6: Commit**

```bash
git add frontend/
git commit -m "feat(frontend): add React Router v6 with route structure"
```

---

## Phase 2: Layout Skeleton

### Task 2.1: Create Desktop Sidebar

**Files:**
- Create: `frontend/src/components/layout/Sidebar.tsx`
- Modify: `frontend/src/components/layout/AppLayout.tsx`

- [ ] **Step 1: Create Sidebar component**

Create `frontend/src/components/layout/Sidebar.tsx`:

```typescript
import { useState, useEffect } from 'react';
import { Link, useLocation } from 'react-router-dom';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import {
  LayoutDashboard,
  Signal,
  Network,
  Globe,
  Shield,
  FileText,
  ChevronLeft,
  ChevronRight,
  LogOut,
  Sun,
  Moon,
} from 'lucide-react';
import { useTheme } from '@/theme/ThemeProvider';

interface NavItem {
  label: string;
  icon: React.ReactNode;
  href: string;
}

interface NavGroup {
  label: string;
  items: NavItem[];
}

const navGroups: NavGroup[] = [
  {
    label: 'Overview',
    items: [
      { label: 'Dashboard', icon: <LayoutDashboard className="h-4 w-4" />, href: '/dashboard' },
    ],
  },
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
      { label: 'Shadowsocks', icon: <Shield className="h-4 w-4" />, href: '/shadowsocks' },
      { label: 'Trojan', icon: <Shield className="h-4 w-4" />, href: '/trojan' },
    ],
  },
  {
    label: 'System',
    items: [
      { label: 'Logs', icon: <FileText className="h-4 w-4" />, href: '/logs' },
    ],
  },
];

interface SidebarProps {
  onLogout: () => void;
}

export function Sidebar({ onLogout }: SidebarProps) {
  const [collapsed, setCollapsed] = useState(() => {
    return localStorage.getItem('sidebar-collapsed') === 'true';
  });
  const location = useLocation();
  const { theme, setTheme } = useTheme();

  useEffect(() => {
    localStorage.setItem('sidebar-collapsed', String(collapsed));
  }, [collapsed]);

  const toggleTheme = () => {
    setTheme(theme === 'dark' ? 'light' : 'dark');
  };

  return (
    <aside
      className={cn(
        'fixed left-0 top-0 z-40 h-screen border-r bg-card transition-all duration-300',
        collapsed ? 'w-16' : 'w-64'
      )}
    >
      <div className="flex h-full flex-col">
        {/* Header */}
        <div className="flex h-14 items-center justify-between border-b px-4">
          {!collapsed && (
            <Link to="/" className="flex items-center gap-2 font-semibold">
              <Shield className="h-6 w-6" />
              <span>Rust Tunnel</span>
            </Link>
          )}
          <Button
            variant="ghost"
            size="icon"
            onClick={() => setCollapsed(!collapsed)}
            className="h-8 w-8"
          >
            {collapsed ? <ChevronRight className="h-4 w-4" /> : <ChevronLeft className="h-4 w-4" />}
          </Button>
        </div>

        {/* Navigation */}
        <ScrollArea className="flex-1 py-4">
          <nav className="space-y-2 px-2">
            {navGroups.map((group) => (
              <Collapsible key={group.label} defaultOpen>
                {!collapsed && (
                  <CollapsibleTrigger className="flex w-full items-center justify-between px-2 py-1 text-sm font-medium text-muted-foreground hover:text-foreground">
                    {group.label}
                  </CollapsibleTrigger>
                )}
                <CollapsibleContent>
                  <div className="space-y-1">
                    {group.items.map((item) => (
                      <TooltipProvider key={item.href}>
                        <Tooltip delayDuration={0}>
                          <TooltipTrigger asChild>
                            <Link
                              to={item.href}
                              className={cn(
                                'flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium hover:bg-accent hover:text-accent-foreground',
                                location.pathname === item.href
                                  ? 'bg-accent text-accent-foreground'
                                  : 'text-muted-foreground'
                              )}
                            >
                              {item.icon}
                              {!collapsed && <span>{item.label}</span>}
                            </Link>
                          </TooltipTrigger>
                          {collapsed && (
                            <TooltipContent side="right">
                              <p>{item.label}</p>
                            </TooltipContent>
                          )}
                        </Tooltip>
                      </TooltipProvider>
                    ))}
                  </div>
                </CollapsibleContent>
              </Collapsible>
            ))}
          </nav>
        </ScrollArea>

        {/* Footer */}
        <div className="border-t p-4 space-y-2">
          <TooltipProvider>
            <Tooltip delayDuration={0}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size={collapsed ? 'icon' : 'default'}
                  className="w-full justify-start"
                  onClick={toggleTheme}
                >
                  {theme === 'dark' ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
                  {!collapsed && <span className="ml-2">Toggle Theme</span>}
                </Button>
              </TooltipTrigger>
              {collapsed && (
                <TooltipContent side="right">
                  <p>Toggle Theme</p>
                </TooltipContent>
              )}
            </Tooltip>
          </TooltipProvider>

          <TooltipProvider>
            <Tooltip delayDuration={0}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size={collapsed ? 'icon' : 'default'}
                  className="w-full justify-start text-destructive hover:text-destructive"
                  onClick={onLogout}
                >
                  <LogOut className="h-4 w-4" />
                  {!collapsed && <span className="ml-2">Logout</span>}
                </Button>
              </TooltipTrigger>
              {collapsed && (
                <TooltipContent side="right">
                  <p>Logout</p>
                </TooltipContent>
              )}
            </Tooltip>
          </TooltipProvider>
        </div>
      </div>
    </aside>
  );
}
```

- [ ] **Step 2: Update AppLayout to use Sidebar**

Update `frontend/src/components/layout/AppLayout.tsx`:

```typescript
import { Outlet, useNavigate } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { useMediaQuery } from '@/hooks/useMediaQuery';

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
```

- [ ] **Step 3: Add cn import to AppLayout**

Update the import in `frontend/src/components/layout/AppLayout.tsx`:

```typescript
import { Outlet, useNavigate } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import { cn } from '@/lib/utils';
```

- [ ] **Step 4: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 5: Commit**

```bash
git add frontend/src/components/layout/
git commit -m "feat(frontend): add desktop sidebar navigation"
```

---

### Task 2.2: Create Mobile Navigation

**Files:**
- Create: `frontend/src/components/layout/MobileNav.tsx`
- Modify: `frontend/src/components/layout/AppLayout.tsx`

- [ ] **Step 1: Create MobileNav component**

Create `frontend/src/components/layout/MobileNav.tsx`:

```typescript
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
  FileText,
  Menu,
  LogOut,
} from 'lucide-react';

const coreTabs = [
  { label: 'Dashboard', icon: <LayoutDashboard className="h-5 w-5" />, href: '/dashboard' },
  { label: 'Quality', icon: <Signal className="h-5 w-5" />, href: '/quality' },
  { label: 'Mesh', icon: <Network className="h-5 w-5" />, href: '/mesh' },
  { label: 'DNS', icon: <Globe className="h-5 w-5" />, href: '/dns' },
];

const moreItems = [
  { label: 'Shadowsocks', icon: <Shield className="h-5 w-5" />, href: '/shadowsocks' },
  { label: 'Trojan', icon: <Shield className="h-5 w-5" />, href: '/trojan' },
  { label: 'Logs', icon: <FileText className="h-5 w-5" />, href: '/logs' },
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
```

- [ ] **Step 2: Update AppLayout to include MobileNav**

Update `frontend/src/components/layout/AppLayout.tsx`:

```typescript
import { Outlet, useNavigate } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { MobileNav } from './MobileNav';
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
      <main
        className={cn(
          'transition-all duration-300',
          isDesktop ? 'pl-64' : 'pb-16'
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
```

- [ ] **Step 3: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 4: Commit**

```bash
git add frontend/src/components/layout/
git commit -m "feat(frontend): add mobile bottom navigation with Sheet"
```

---

### Task 2.3: Create PageHeader Component

**Files:**
- Create: `frontend/src/components/layout/PageHeader.tsx`

- [ ] **Step 1: Create PageHeader component**

Create `frontend/src/components/layout/PageHeader.tsx`:

```typescript
import { cn } from '@/lib/utils';

interface PageHeaderProps {
  title: string;
  description?: string;
  children?: React.ReactNode;
  className?: string;
}

export function PageHeader({ title, description, children, className }: PageHeaderProps) {
  return (
    <div className={cn('flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between', className)}>
      <div>
        <h1 className="text-2xl font-bold tracking-tight">{title}</h1>
        {description && (
          <p className="text-muted-foreground">{description}</p>
        )}
      </div>
      {children && <div className="flex items-center gap-2">{children}</div>}
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/components/layout/PageHeader.tsx
git commit -m "feat(frontend): add PageHeader component"
```

---

## Phase 3: Core Pages

### Task 3.1: Create LoginPage

**Files:**
- Rewrite: `frontend/src/pages/LoginPage.tsx`

- [ ] **Step 1: Create LoginPage component**

Rewrite `frontend/src/pages/LoginPage.tsx`:

```typescript
import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useLogin } from '@/api/hooks';
import { Shield } from 'lucide-react';

export default function LoginPage() {
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const navigate = useNavigate();
  const login = useLogin();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError('');

    try {
      const data = await login.mutateAsync(password);
      localStorage.setItem('auth_token', data.token);
      navigate('/dashboard');
    } catch (err) {
      setError('Invalid password');
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-sm shadow-sm">
        <CardHeader className="text-center">
          <div className="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-full bg-primary">
            <Shield className="h-6 w-6 text-primary-foreground" />
          </div>
          <CardTitle className="text-2xl">Rust Tunnel</CardTitle>
          <CardDescription>Enter your password to continue</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit} className="space-y-4">
            <div className="space-y-2">
              <Input
                type="password"
                placeholder="Password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </div>
            {error && (
              <p className="text-sm text-destructive">{error}</p>
            )}
            <Button type="submit" className="w-full" disabled={login.isPending}>
              {login.isPending ? 'Logging in...' : 'Login'}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/LoginPage.tsx
git commit -m "feat(frontend): implement LoginPage with shadcn Card"
```

---

### Task 3.2: Create StatCard Component

**Files:**
- Rewrite: `frontend/src/components/shared/StatCard.tsx`

- [ ] **Step 1: Create StatCard component**

Rewrite `frontend/src/components/shared/StatCard.tsx`:

```typescript
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { cn } from '@/lib/utils';

interface StatCardProps {
  title: string;
  value: string | number;
  description?: string;
  icon?: React.ReactNode;
  trend?: 'up' | 'down' | 'neutral';
  className?: string;
}

export function StatCard({ title, value, description, icon, trend, className }: StatCardProps) {
  return (
    <Card className={cn('', className)}>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
        {icon && <div className="text-muted-foreground">{icon}</div>}
      </CardHeader>
      <CardContent>
        <div className="text-2xl font-bold">{value}</div>
        {description && (
          <p
            className={cn(
              'text-xs',
              trend === 'up' && 'text-green-500',
              trend === 'down' && 'text-red-500',
              !trend && 'text-muted-foreground'
            )}
          >
            {description}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/components/shared/StatCard.tsx
git commit -m "feat(frontend): rewrite StatCard with shadcn Card"
```

---

### Task 3.3: Create QualityBadge Component

**Files:**
- Create: `frontend/src/components/shared/QualityBadge.tsx`

- [ ] **Step 1: Create QualityBadge component**

Create `frontend/src/components/shared/QualityBadge.tsx`:

```typescript
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';

interface QualityBadgeProps {
  score: number;
  className?: string;
}

export function QualityBadge({ score, className }: QualityBadgeProps) {
  const getVariant = () => {
    if (score >= 80) return 'default';
    if (score >= 60) return 'secondary';
    return 'destructive';
  };

  const getLabel = () => {
    if (score >= 80) return 'Excellent';
    if (score >= 60) return 'Good';
    if (score >= 40) return 'Fair';
    return 'Poor';
  };

  return (
    <Badge variant={getVariant()} className={cn('', className)}>
      {getLabel()} ({score})
    </Badge>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/components/shared/QualityBadge.tsx
git commit -m "feat(frontend): add QualityBadge component"
```

---

### Task 3.4: Create DashboardPage

**Files:**
- Rewrite: `frontend/src/pages/DashboardPage.tsx`

- [ ] **Step 1: Create DashboardPage component**

Rewrite `frontend/src/pages/DashboardPage.tsx`:

```typescript
import { useNavigate } from 'react-router-dom';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients, useMetrics } from '@/api/hooks';
import { Users, Activity, ArrowDown, ArrowUp, ExternalLink } from 'lucide-react';

export default function DashboardPage() {
  const navigate = useNavigate();
  const { data: clients, isLoading: clientsLoading } = useClients();
  const { data: metrics, isLoading: metricsLoading } = useMetrics();

  const connectedClients = clients?.filter((c: any) => c.connected).length ?? 0;
  const activeConnections = metrics?.active_connections ?? 0;
  const totalBytesIn = metrics?.total_bytes_in ?? 0;
  const totalBytesOut = metrics?.total_bytes_out ?? 0;

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Dashboard"
        description="Overview of your tunnel connections"
      />

      {/* Stats Grid */}
      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
        <StatCard
          title="Connected Clients"
          value={connectedClients}
          icon={<Users className="h-4 w-4" />}
        />
        <StatCard
          title="Active Connections"
          value={activeConnections}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="Total Bytes In"
          value={formatBytes(totalBytesIn)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Total Bytes Out"
          value={formatBytes(totalBytesOut)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Client List */}
      <Card>
        <CardHeader>
          <CardTitle>Clients</CardTitle>
        </CardHeader>
        <CardContent>
          {clientsLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : clients?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No clients connected</div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Quality</TableHead>
                  <TableHead>RTT</TableHead>
                  <TableHead>Loss</TableHead>
                  <TableHead>Connections</TableHead>
                  <TableHead>Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients?.map((client: any) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium">{client.port}</TableCell>
                    <TableCell>
                      <QualityBadge score={client.quality ?? 0} />
                    </TableCell>
                    <TableCell>{client.rtt ? `${client.rtt}ms` : '-'}</TableCell>
                    <TableCell>{client.loss ? `${client.loss}%` : '-'}</TableCell>
                    <TableCell>{client.connections ?? 0}</TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => navigate(`/clients/${client.port}`)}
                      >
                        <ExternalLink className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/DashboardPage.tsx
git commit -m "feat(frontend): implement DashboardPage with shadcn components"
```

---

### Task 3.5: Create ClientDetailPage

**Files:**
- Create: `frontend/src/pages/ClientDetailPage.tsx`

- [ ] **Step 1: Create ClientDetailPage component**

Create `frontend/src/pages/ClientDetailPage.tsx`:

```typescript
import { useParams, useNavigate } from 'react-router-dom';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients, useQuality, useTraffic } from '@/api/hooks';
import { ArrowLeft, Signal, Clock, Activity } from 'lucide-react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';

export default function ClientDetailPage() {
  const { port } = useParams<{ port: string }>();
  const navigate = useNavigate();
  const portNum = parseInt(port || '0', 10);

  const { data: clients } = useClients();
  const { data: quality } = useQuality(portNum);
  const { data: traffic } = useTraffic(portNum, 24);

  const client = clients?.find((c: any) => c.port === portNum);

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  return (
    <div className="space-y-6">
      <PageHeader title={`Client Port ${port}`}>
        <Button variant="outline" onClick={() => navigate('/dashboard')}>
          <ArrowLeft className="mr-2 h-4 w-4" />
          Back
        </Button>
      </PageHeader>

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Quality Score"
          value={quality?.score ?? '-'}
          icon={<Signal className="h-4 w-4" />}
        />
        <StatCard
          title="RTT"
          value={quality?.rtt ? `${quality.rtt}ms` : '-'}
          icon={<Clock className="h-4 w-4" />}
        />
        <StatCard
          title="Active Connections"
          value={client?.connections ?? 0}
          icon={<Activity className="h-4 w-4" />}
        />
      </div>

      {/* Traffic Chart */}
      <Card>
        <CardHeader>
          <CardTitle>Traffic (Last 24h)</CardTitle>
        </CardHeader>
        <CardContent>
          {traffic?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No traffic data</div>
          ) : (
            <ResponsiveContainer width="100%" height={300}>
              <LineChart data={traffic ?? []}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis tickFormatter={(v) => formatBytes(v)} />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                  formatter={(value: number) => formatBytes(value)}
                />
                <Line
                  type="monotone"
                  dataKey="bytes_in"
                  stroke="#3b82f6"
                  name="Bytes In"
                />
                <Line
                  type="monotone"
                  dataKey="bytes_out"
                  stroke="#10b981"
                  name="Bytes Out"
                />
              </LineChart>
            </ResponsiveContainer>
          )}
        </CardContent>
      </Card>

      {/* Quality History */}
      <Card>
        <CardHeader>
          <CardTitle>Quality History</CardTitle>
        </CardHeader>
        <CardContent>
          {quality?.history?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No quality history</div>
          ) : (
            <ResponsiveContainer width="100%" height={200}>
              <LineChart data={quality?.history ?? []}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis domain={[0, 100]} />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                />
                <Line
                  type="monotone"
                  dataKey="score"
                  stroke="#8b5cf6"
                  name="Quality Score"
                />
              </LineChart>
            </ResponsiveContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/ClientDetailPage.tsx
git commit -m "feat(frontend): implement ClientDetailPage with charts"
```

---

### Task 3.6: Create QualityPage

**Files:**
- Rewrite: `frontend/src/pages/QualityPage.tsx`

- [ ] **Step 1: Create QualityPage component**

Rewrite `frontend/src/pages/QualityPage.tsx`:

```typescript
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQualitySummary } from '@/api/hooks';
import { Signal, AlertTriangle, Activity } from 'lucide-react';
import { cn } from '@/lib/utils';

export default function QualityPage() {
  const { data: summary, isLoading } = useQualitySummary();

  const totalConnections = summary?.total_connections ?? 0;
  const warningCount = summary?.warning_count ?? 0;
  const averageScore = summary?.average_score ?? 0;

  const getHeatmapColor = (score: number) => {
    if (score >= 80) return 'bg-green-500/20';
    if (score >= 60) return 'bg-yellow-500/20';
    if (score >= 40) return 'bg-orange-500/20';
    return 'bg-red-500/20';
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Connection Quality"
        description="Monitor connection quality across all clients"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Total Connections"
          value={totalConnections}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="Warnings"
          value={warningCount}
          icon={<AlertTriangle className="h-4 w-4" />}
          trend={warningCount > 0 ? 'down' : 'neutral'}
        />
        <StatCard
          title="Average Quality"
          value={averageScore.toFixed(1)}
          icon={<Signal className="h-4 w-4" />}
        />
      </div>

      {/* Quality Heatmap */}
      <Card>
        <CardHeader>
          <CardTitle>Quality Heatmap</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : !summary?.clients?.length ? (
            <div className="text-center py-8 text-muted-foreground">No clients</div>
          ) : (
            <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
              {summary.clients.map((client: any) => (
                <div
                  key={client.port}
                  className={cn(
                    'rounded-lg border p-3 text-center',
                    getHeatmapColor(client.score)
                  )}
                >
                  <div className="text-sm font-medium">Port {client.port}</div>
                  <div className="text-2xl font-bold">{client.score}</div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Worst Connections */}
      <Card>
        <CardHeader>
          <CardTitle>Worst Connections</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : !summary?.worst?.length ? (
            <div className="text-center py-8 text-muted-foreground">No data</div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Quality</TableHead>
                  <TableHead>RTT</TableHead>
                  <TableHead>Loss</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {summary.worst.map((client: any) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium">{client.port}</TableCell>
                    <TableCell>
                      <QualityBadge score={client.score} />
                    </TableCell>
                    <TableCell>{client.rtt ? `${client.rtt}ms` : '-'}</TableCell>
                    <TableCell>{client.loss ? `${client.loss}%` : '-'}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/QualityPage.tsx
git commit -m "feat(frontend): implement QualityPage with heatmap"
```

---

## Phase 4: Remaining Pages

### Task 4.1: Create ShadowsocksPage

**Files:**
- Rewrite: `frontend/src/pages/ShadowsocksPage.tsx`

- [ ] **Step 1: Create ShadowsocksPage component**

Rewrite `frontend/src/pages/ShadowsocksPage.tsx`:

```typescript
import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useShadowsocksConfig, useUpdateShadowsocksConfig } from '@/api/hooks';
import { Shield, Activity, ArrowDown, ArrowUp } from 'lucide-react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';

export default function ShadowsocksPage() {
  const { data: config, isLoading } = useShadowsocksConfig();
  const updateConfig = useUpdateShadowsocksConfig();

  const [enabled, setEnabled] = useState(false);
  const [port, setPort] = useState('');
  const [password, setPassword] = useState('');
  const [cipher, setCipher] = useState('aes-256-gcm');

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled ?? false);
      setPort(config.port?.toString() ?? '');
      setPassword(config.password ?? '');
      setCipher(config.cipher ?? 'aes-256-gcm');
    }
  }, [config]);

  const handleSave = () => {
    updateConfig.mutate({
      enabled,
      port: parseInt(port, 10),
      password,
      cipher,
    });
  };

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Shadowsocks"
        description="Configure Shadowsocks proxy server"
      />

      {/* Config Card */}
      <Card>
        <CardHeader>
          <CardTitle>Configuration</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : (
            <>
              <div className="flex items-center justify-between">
                <div>
                  <div className="font-medium">Enable Shadowsocks</div>
                  <div className="text-sm text-muted-foreground">Start the Shadowsocks proxy server</div>
                </div>
                <Switch checked={enabled} onCheckedChange={setEnabled} />
              </div>

              <div className="grid gap-4 md:grid-cols-2">
                <div className="space-y-2">
                  <label className="text-sm font-medium">Port</label>
                  <Input
                    type="number"
                    value={port}
                    onChange={(e) => setPort(e.target.value)}
                    placeholder="8388"
                  />
                </div>
                <div className="space-y-2">
                  <label className="text-sm font-medium">Password</label>
                  <Input
                    type="password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    placeholder="Enter password"
                  />
                </div>
              </div>

              <div className="space-y-2">
                <label className="text-sm font-medium">Cipher</label>
                <Select value={cipher} onValueChange={setCipher}>
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="aes-256-gcm">AES-256-GCM</SelectItem>
                    <SelectItem value="chacha20-ietf-poly1305">ChaCha20-IETF-Poly1305</SelectItem>
                  </SelectContent>
                </Select>
              </div>

              <Button onClick={handleSave} disabled={updateConfig.isPending}>
                {updateConfig.isPending ? 'Saving...' : 'Save Configuration'}
              </Button>
            </>
          )}
        </CardContent>
      </Card>

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Status"
          value={enabled ? 'Active' : 'Inactive'}
          icon={<Shield className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes In"
          value={formatBytes(config?.bytes_in ?? 0)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes Out"
          value={formatBytes(config?.bytes_out ?? 0)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle>Throughput</CardTitle>
        </CardHeader>
        <CardContent>
          {!config?.throughput?.length ? (
            <div className="text-center py-8 text-muted-foreground">No data</div>
          ) : (
            <ResponsiveContainer width="100%" height={300}>
              <LineChart data={config.throughput}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis tickFormatter={(v) => formatBytes(v)} />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                  formatter={(value: number) => formatBytes(value)}
                />
                <Line type="monotone" dataKey="bytes_in" stroke="#3b82f6" name="Bytes In" />
                <Line type="monotone" dataKey="bytes_out" stroke="#10b981" name="Bytes Out" />
              </LineChart>
            </ResponsiveContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/ShadowsocksPage.tsx
git commit -m "feat(frontend): implement ShadowsocksPage with config form"
```

---

### Task 4.2: Create TrojanPage

**Files:**
- Rewrite: `frontend/src/pages/TrojanPage.tsx`

- [ ] **Step 1: Create TrojanPage component**

Rewrite `frontend/src/pages/TrojanPage.tsx`:

```typescript
import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useTrojanConfig, useUpdateTrojanConfig } from '@/api/hooks';
import { Shield, Activity, ArrowDown, ArrowUp } from 'lucide-react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';

export default function TrojanPage() {
  const { data: config, isLoading } = useTrojanConfig();
  const updateConfig = useUpdateTrojanConfig();

  const [enabled, setEnabled] = useState(false);
  const [port, setPort] = useState('');
  const [password, setPassword] = useState('');

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled ?? false);
      setPort(config.port?.toString() ?? '');
      setPassword(config.password ?? '');
    }
  }, [config]);

  const handleSave = () => {
    updateConfig.mutate({
      enabled,
      port: parseInt(port, 10),
      password,
    });
  };

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Trojan"
        description="Configure Trojan proxy server"
      />

      {/* Config Card */}
      <Card>
        <CardHeader>
          <CardTitle>Configuration</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : (
            <>
              <div className="flex items-center justify-between">
                <div>
                  <div className="font-medium">Enable Trojan</div>
                  <div className="text-sm text-muted-foreground">Start the Trojan proxy server</div>
                </div>
                <Switch checked={enabled} onCheckedChange={setEnabled} />
              </div>

              <div className="grid gap-4 md:grid-cols-2">
                <div className="space-y-2">
                  <label className="text-sm font-medium">Port</label>
                  <Input
                    type="number"
                    value={port}
                    onChange={(e) => setPort(e.target.value)}
                    placeholder="443"
                  />
                </div>
                <div className="space-y-2">
                  <label className="text-sm font-medium">Password</label>
                  <Input
                    type="password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    placeholder="Enter password"
                  />
                </div>
              </div>

              <Button onClick={handleSave} disabled={updateConfig.isPending}>
                {updateConfig.isPending ? 'Saving...' : 'Save Configuration'}
              </Button>
            </>
          )}
        </CardContent>
      </Card>

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Status"
          value={enabled ? 'Active' : 'Inactive'}
          icon={<Shield className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes In"
          value={formatBytes(config?.bytes_in ?? 0)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Bytes Out"
          value={formatBytes(config?.bytes_out ?? 0)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Throughput Chart */}
      <Card>
        <CardHeader>
          <CardTitle>Throughput</CardTitle>
        </CardHeader>
        <CardContent>
          {!config?.throughput?.length ? (
            <div className="text-center py-8 text-muted-foreground">No data</div>
          ) : (
            <ResponsiveContainer width="100%" height={300}>
              <LineChart data={config.throughput}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis
                  dataKey="timestamp"
                  tickFormatter={(ts) => new Date(ts).toLocaleTimeString()}
                />
                <YAxis tickFormatter={(v) => formatBytes(v)} />
                <Tooltip
                  labelFormatter={(ts) => new Date(ts).toLocaleString()}
                  formatter={(value: number) => formatBytes(value)}
                />
                <Line type="monotone" dataKey="bytes_in" stroke="#3b82f6" name="Bytes In" />
                <Line type="monotone" dataKey="bytes_out" stroke="#10b981" name="Bytes Out" />
              </LineChart>
            </ResponsiveContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/TrojanPage.tsx
git commit -m "feat(frontend): implement TrojanPage with config form"
```

---

### Task 4.3: Create MeshPage

**Files:**
- Rewrite: `frontend/src/pages/MeshPage.tsx`

- [ ] **Step 1: Create MeshPage component**

Rewrite `frontend/src/pages/MeshPage.tsx`:

```typescript
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Badge } from '@/components/ui/badge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients } from '@/api/hooks';
import { Network } from 'lucide-react';

export default function MeshPage() {
  const { data: clients, isLoading } = useClients();

  return (
    <div className="space-y-6">
      <PageHeader
        title="Mesh Network"
        description="View mesh network connections and members"
      />

      <Card>
        <CardHeader>
          <CardTitle>Mesh Members</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : !clients?.length ? (
            <div className="text-center py-8 text-muted-foreground">No mesh members</div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Connections</TableHead>
                  <TableHead>Services</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients.map((client: any) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium">{client.port}</TableCell>
                    <TableCell>
                      <Badge variant={client.connected ? 'default' : 'secondary'}>
                        {client.connected ? 'Online' : 'Offline'}
                      </Badge>
                    </TableCell>
                    <TableCell>{client.connections ?? 0}</TableCell>
                    <TableCell>{client.services?.join(', ') ?? '-'}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/MeshPage.tsx
git commit -m "feat(frontend): implement MeshPage with member table"
```

---

### Task 4.4: Create DnsPage

**Files:**
- Rewrite: `frontend/src/pages/DnsPage.tsx`

- [ ] **Step 1: Create DnsPage component**

Rewrite `frontend/src/pages/DnsPage.tsx`:

```typescript
import { useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogTrigger } from '@/components/ui/dialog';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '@/api/client';
import { Plus, Trash2 } from 'lucide-react';

export default function DnsPage() {
  const queryClient = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [newRecord, setNewRecord] = useState({ name: '', type: 'A', value: '' });

  const { data: records, isLoading } = useQuery({
    queryKey: ['dns-records'],
    queryFn: () => api.get('/api/dns/records').then(res => res.data),
  });

  const addRecord = useMutation({
    mutationFn: (record: typeof newRecord) => api.post('/api/dns/records', record),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
      setDialogOpen(false);
      setNewRecord({ name: '', type: 'A', value: '' });
    },
  });

  const deleteRecord = useMutation({
    mutationFn: (id: string) => api.delete(`/api/dns/records/${id}`),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
    },
  });

  const handleAdd = (e: React.FormEvent) => {
    e.preventDefault();
    addRecord.mutate(newRecord);
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="DNS Records"
        description="Manage DNS records for the tunnel"
      >
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button>
              <Plus className="mr-2 h-4 w-4" />
              Add Record
            </Button>
          </DialogTrigger>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Add DNS Record</DialogTitle>
            </DialogHeader>
            <form onSubmit={handleAdd} className="space-y-4">
              <div className="space-y-2">
                <label className="text-sm font-medium">Name</label>
                <Input
                  value={newRecord.name}
                  onChange={(e) => setNewRecord({ ...newRecord, name: e.target.value })}
                  placeholder="example.com"
                  required
                />
              </div>
              <div className="space-y-2">
                <label className="text-sm font-medium">Type</label>
                <select
                  className="w-full rounded-md border bg-background px-3 py-2"
                  value={newRecord.type}
                  onChange={(e) => setNewRecord({ ...newRecord, type: e.target.value })}
                >
                  <option value="A">A</option>
                  <option value="AAAA">AAAA</option>
                  <option value="CNAME">CNAME</option>
                </select>
              </div>
              <div className="space-y-2">
                <label className="text-sm font-medium">Value</label>
                <Input
                  value={newRecord.value}
                  onChange={(e) => setNewRecord({ ...newRecord, value: e.target.value })}
                  placeholder="192.168.1.1"
                  required
                />
              </div>
              <Button type="submit" disabled={addRecord.isPending}>
                {addRecord.isPending ? 'Adding...' : 'Add Record'}
              </Button>
            </form>
          </DialogContent>
        </Dialog>
      </PageHeader>

      <Card>
        <CardHeader>
          <CardTitle>Records</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : !records?.length ? (
            <div className="text-center py-8 text-muted-foreground">No DNS records</div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Value</TableHead>
                  <TableHead>Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {records.map((record: any) => (
                  <TableRow key={record.id}>
                    <TableCell className="font-medium">{record.name}</TableCell>
                    <TableCell>{record.type}</TableCell>
                    <TableCell>{record.value}</TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => {
                          if (confirm('Delete this record?')) {
                            deleteRecord.mutate(record.id);
                          }
                        }}
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/DnsPage.tsx
git commit -m "feat(frontend): implement DnsPage with Dialog form"
```

---

### Task 4.5: Create LogsPage

**Files:**
- Rewrite: `frontend/src/pages/LogsPage.tsx`

- [ ] **Step 1: Create LogsPage component**

Rewrite `frontend/src/pages/LogsPage.tsx`:

```typescript
import { useState, useEffect, useRef } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Badge } from '@/components/ui/badge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useLogs } from '@/api/hooks';
import { Pause, Play, RefreshCw } from 'lucide-react';
import { cn } from '@/lib/utils';

export default function LogsPage() {
  const [level, setLevel] = useState<string>('all');
  const [search, setSearch] = useState('');
  const [paused, setPaused] = useState(false);
  const [page, setPage] = useState(1);
  const logsEndRef = useRef<HTMLDivElement>(null);

  const { data, isLoading, refetch } = useLogs(page, 50, level === 'all' ? undefined : level);

  const logs = data?.logs ?? [];
  const total = data?.total ?? 0;
  const hasMore = page * 50 < total;

  const filteredLogs = search
    ? logs.filter((log: any) =>
        log.message.toLowerCase().includes(search.toLowerCase()) ||
        log.target?.toLowerCase().includes(search.toLowerCase())
      )
    : logs;

  const getLevelVariant = (level: string) => {
    switch (level.toUpperCase()) {
      case 'ERROR':
        return 'destructive';
      case 'WARN':
        return 'secondary';
      case 'INFO':
        return 'default';
      case 'DEBUG':
        return 'outline';
      default:
        return 'outline';
    }
  };

  useEffect(() => {
    if (!paused && logsEndRef.current) {
      logsEndRef.current.scrollIntoView({ behavior: 'smooth' });
    }
  }, [logs, paused]);

  return (
    <div className="space-y-6">
      <PageHeader
        title="Logs"
        description="View real-time application logs"
      >
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={() => refetch()}>
            <RefreshCw className="h-4 w-4" />
          </Button>
          <Button
            variant={paused ? 'default' : 'outline'}
            size="sm"
            onClick={() => setPaused(!paused)}
          >
            {paused ? <Play className="h-4 w-4" /> : <Pause className="h-4 w-4" />}
          </Button>
        </div>
      </PageHeader>

      {/* Filters */}
      <div className="flex flex-col gap-4 sm:flex-row">
        <Select value={level} onValueChange={setLevel}>
          <SelectTrigger className="w-full sm:w-32">
            <SelectValue placeholder="Level" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All Levels</SelectItem>
            <SelectItem value="ERROR">Error</SelectItem>
            <SelectItem value="WARN">Warning</SelectItem>
            <SelectItem value="INFO">Info</SelectItem>
            <SelectItem value="DEBUG">Debug</SelectItem>
          </SelectContent>
        </Select>
        <Input
          placeholder="Search logs..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="flex-1"
        />
      </div>

      {/* Log Entries */}
      <Card>
        <CardHeader>
          <CardTitle>Log Entries</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">Loading...</div>
          ) : filteredLogs.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">No logs</div>
          ) : (
            <div className="space-y-1 font-mono text-sm">
              {filteredLogs.map((log: any, index: number) => (
                <div
                  key={index}
                  className={cn(
                    'flex items-start gap-2 rounded-md p-2 hover:bg-muted',
                    log.level === 'ERROR' && 'bg-destructive/10'
                  )}
                >
                  <Badge variant={getLevelVariant(log.level)} className="mt-0.5 shrink-0">
                    {log.level}
                  </Badge>
                  <div className="flex-1 min-w-0">
                    <div className="text-muted-foreground text-xs">
                      {new Date(log.timestamp).toLocaleString()}
                      {log.target && <span className="ml-2">[{log.target}]</span>}
                    </div>
                    <div className="break-all">{log.message}</div>
                  </div>
                </div>
              ))}
              <div ref={logsEndRef} />
            </div>
          )}

          {hasMore && (
            <div className="mt-4 text-center">
              <Button variant="outline" onClick={() => setPage(page + 1)}>
                Load More
              </Button>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/LogsPage.tsx
git commit -m "feat(frontend): implement LogsPage with SSE and filters"
```

---

## Phase 5: Cleanup and Testing

### Task 5.1: Remove Old Components

**Files:**
- Delete: `frontend/src/components/Navbar.tsx`
- Delete: `frontend/src/components/Dashboard.tsx`
- Delete: `frontend/src/components/Login.tsx`
- Delete: `frontend/src/components/shared/MobileBottomNav.tsx`

- [ ] **Step 1: Remove old component files**

```bash
rm frontend/src/components/Navbar.tsx
rm frontend/src/components/Dashboard.tsx
rm frontend/src/components/Login.tsx
rm frontend/src/components/shared/MobileBottomNav.tsx
```

- [ ] **Step 2: Verify build still succeeds**

```bash
cd frontend
npm run build
```

Expected: Build succeeds (no remaining imports to deleted files)

- [ ] **Step 3: Commit**

```bash
git add frontend/
git commit -m "chore(frontend): remove old hand-built components"
```

---

### Task 5.2: Update ChartContainer Component

**Files:**
- Rewrite: `frontend/src/components/shared/ChartContainer.tsx`

- [ ] **Step 1: Rewrite ChartContainer with shadcn Card**

Rewrite `frontend/src/components/shared/ChartContainer.tsx`:

```typescript
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { cn } from '@/lib/utils';

interface ChartContainerProps {
  title: string;
  children: React.ReactNode;
  className?: string;
  description?: string;
}

export function ChartContainer({ title, children, className, description }: ChartContainerProps) {
  return (
    <Card className={cn('', className)}>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        {description && <p className="text-sm text-muted-foreground">{description}</p>}
      </CardHeader>
      <CardContent>{children}</CardContent>
    </Card>
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 3: Commit**

```bash
git add frontend/src/components/shared/ChartContainer.tsx
git commit -m "feat(frontend): rewrite ChartContainer with shadcn Card"
```

---

### Task 5.3: Run Full Build and Lint

**Files:**
- None (verification only)

- [ ] **Step 1: Run TypeScript type check**

```bash
cd frontend
npx tsc --noEmit
```

Expected: No errors

- [ ] **Step 2: Run ESLint**

```bash
cd frontend
npm run lint
```

Expected: No warnings or errors

- [ ] **Step 3: Run full build**

```bash
cd frontend
npm run build
```

Expected: Build succeeds

- [ ] **Step 4: Run tests**

```bash
cd frontend
npm test
```

Expected: All tests pass

- [ ] **Step 5: Final commit**

```bash
git add frontend/
git commit -m "chore(frontend): final cleanup and verification"
```

---

## Summary

Total Tasks: 15
Total Steps: ~75

**Phase 1 (Infrastructure):** 3 tasks - shadcn/ui setup, React Query v5, React Router v6
**Phase 2 (Layout):** 3 tasks - Sidebar, MobileNav, PageHeader
**Phase 3 (Core Pages):** 6 tasks - Login, StatCard, QualityBadge, Dashboard, ClientDetail, Quality
**Phase 4 (Remaining Pages):** 5 tasks - Shadowsocks, Trojan, Mesh, DNS, Logs
**Phase 5 (Cleanup):** 3 tasks - Remove old files, update ChartContainer, final verification

Each task produces a working, testable increment. The application remains functional throughout the migration.
