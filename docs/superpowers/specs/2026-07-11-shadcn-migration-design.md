# Frontend shadcn/ui Migration Design

## Overview

Migrate the rust-tunnel frontend from hand-built Tailwind components to shadcn/ui, redesign the layout to a sidebar-based dashboard, and upgrade the data/routing layer.

## Requirements

- Replace all hand-built UI components with shadcn/ui equivalents
- Desktop: left sidebar navigation with collapsible groups
- Mobile: bottom tab bar + "More" sheet for overflow pages
- Color scheme: shadcn default (zinc neutral + blue accent)
- Login page: minimal centered card
- Upgrade React Query v3 to v5 (TanStack Query)
- Add react-router-dom v6 for real routing
- Rewrite all 8 page components with shadcn components

## Migration Strategy: Layered Incremental (4 Phases)

### Phase 1: Infrastructure

**shadcn/ui setup:**
- Run `npx shadcn@latest init` with default zinc style, CSS variables enabled
- Add `tailwindcss-animate` plugin to tailwind.config.js
- Add `@/*` path alias in tsconfig.json pointing to `src/`
- Install components: Button, Card, Input, Table, Badge, Sheet, Collapsible, Separator, DropdownMenu, Tooltip, Skeleton, Tabs, Select, Switch, ScrollArea

**React Query v3 → v5:**
- Replace `react-query` with `@tanstack/react-query`
- Migrate API: `useQuery('key', fn)` → `useQuery({ queryKey: ['key'], queryFn: fn })`
- Migrate mutations: `useMutation(fn)` → `useMutation({ mutationFn: fn })`
- Update `invalidateQueries` calls to object syntax
- Update all imports across every component file

**React Router v6:**
- Install `react-router-dom`
- Route structure:
  ```
  /login              → LoginPage
  /                   → AppLayout (Sidebar wrapper)
    /dashboard        → DashboardPage
    /quality          → QualityPage
    /mesh             → MeshPage
    /dns              → DnsPage
    /shadowsocks      → ShadowsocksPage
    /trojan           → TrojanPage
    /logs             → LogsPage
    /clients/:port    → ClientDetailPage
  ```
- Auth guard: ProtectedRoute component wrapping the AppLayout, redirects to /login if unauthenticated
- Replace state-based page switching in Dashboard with `<Outlet />`

### Phase 2: Layout Skeleton

**Sidebar (desktop):**
- Fixed left sidebar, 256px expanded, 64px collapsed (icon-only)
- Header: logo/title + collapse toggle button
- Navigation groups using shadcn Collapsible:
  - Overview: Dashboard
  - Network: Quality, Mesh, DNS
  - Proxy: Shadowsocks, Trojan
  - System: Logs
- Active page highlighted with `bg-accent`
- Footer (fixed bottom): ThemeToggle + Logout button
- Collapse state persisted to localStorage

**Mobile navigation:**
- Sidebar hidden on mobile
- Bottom tab bar with 5 core tabs + "More" button
- "More" opens a shadcn Sheet with remaining pages
- Tab bar styled with shadcn components

**Page header bar:**
- Each page has a header showing page title + right-side actions
- Mobile header includes hamburger menu button

### Phase 3: Core Pages

**Login page:**
- Centered Card component
- Title "Rust Tunnel" + subtitle
- shadcn Input (password) + Button (submit)
- Error message display
- Plain `bg-background` background, Card with `shadow-sm` + `border`

**Dashboard page:**
- Top row: 4 StatCards using shadcn Card
  - Connected Clients, Active Connections, Total Bytes In, Total Bytes Out
- Middle: ClientList
  - Desktop: shadcn Table with columns (Port, Quality, RTT, Loss, Connections, Actions)
  - Mobile: Card list with same data
- Bottom: TrafficChart (recharts inside shadcn Card)
- ClientDetail moved to `/clients/:port` route (was modal), contains quality gauge + traffic chart + quality history

**Quality page:**
- Top StatCards: total connections, warning count, average quality score
- Quality heatmap grid: shadcn Card per cell with color-coded background
- Worst connections table: shadcn Table + Badge for quality level

### Phase 4: Remaining Pages

**Shadowsocks / Trojan pages:**
- Config card: enabled Switch, port, cipher/fallback fields
- Stats card: traffic, connection counts
- Throughput chart: recharts inside shadcn Card
- Quality history chart

**Mesh page:**
- Mesh list as Card grid
- Member/service details in Table or nested Cards

**DNS page:**
- Records list in shadcn Table
- Add form in shadcn Dialog (replaces inline form)
- Delete with confirmation

**Logs page:**
- SSE real-time stream preserved
- Log entries with monospace font + shadcn Badge for level (ERROR=red, WARN=yellow, INFO=blue, DEBUG=gray)
- Filter bar: shadcn Select (level) + Input (search)
- Pause/resume Button
- Load more Button for pagination

## Component Mapping

| Current | shadcn/ui Replacement |
|---------|----------------------|
| StatCard (custom) | Card + custom styling |
| Navbar (custom) | Sidebar + Sheet (mobile) |
| MobileBottomNav (custom) | Tabs-style bottom nav |
| ClientList table | Table |
| QualityIndicator | Badge |
| ChartContainer (custom) | Card wrapper |
| TimeRangeSelector (custom) | Select + custom inputs |
| Login form (custom) | Card + Input + Button |
| ThemeToggle (custom) | Button variant="ghost" |
| Inline forms (DNS, SS, Trojan) | Dialog + form |

## File Structure (after migration)

```
frontend/src/
  components/
    ui/               # shadcn components (auto-generated)
    layout/
      AppLayout.tsx    # Sidebar + Outlet wrapper
      Sidebar.tsx      # Desktop sidebar
      MobileNav.tsx    # Bottom tab + More sheet
      PageHeader.tsx   # Page title bar
    shared/
      StatCard.tsx     # shadcn Card-based stat display
      ChartContainer.tsx # Card wrapper for charts
      QualityBadge.tsx # Quality score badge
  pages/
    LoginPage.tsx
    DashboardPage.tsx
    QualityPage.tsx
    MeshPage.tsx
    DnsPage.tsx
    ShadowsocksPage.tsx
    TrojanPage.tsx
    LogsPage.tsx
    ClientDetailPage.tsx
  hooks/
    useTimeRange.ts
    useMediaQuery.ts
  api/
    client.ts
  types/
    index.ts
  lib/
    utils.ts          # shadcn cn() utility
  theme/
    ThemeProvider.tsx
  App.tsx             # Router setup
  main.tsx
  index.css           # shadcn CSS variables
```

## Key Decisions

- **No new routing library beyond react-router-dom v6** — sufficient for this scale
- **Keep recharts** — it works well, no need to switch chart libraries
- **Keep axios** — API client stays as-is, only React Query wrapper changes
- **ClientDetail as route, not modal** — better UX with deep linking and back button
- **Preserve SSE for logs** — EventSource pattern stays, only UI changes
- **localStorage for sidebar collapse state** — simple, no server round-trip
