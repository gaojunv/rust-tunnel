# Frontend 图表优化与移动端适配 - 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为所有图表添加时间区间选择功能，实现移动端响应式布局（底部标签栏），并提取共享组件消除代码重复。

**Architecture:** 新增 `components/shared/`、`hooks/`、`utils/` 三个模块存放共享代码。先建基础设施（工具函数、hooks），再建共享 UI 组件，最后逐页重构现有页面。所有修改保持 Recharts + Tailwind + React Query 技术栈不变。

**Tech Stack:** React 18, TypeScript, Tailwind CSS 3, Recharts 2, React Query 3

**验证方式:** 每完成一个任务后运行 `cd frontend && npm run build` 确保 TypeScript 编译和 Vite 构建通过。

---

## 文件结构总览

### 新建文件
- `frontend/src/utils/format.ts` — 共享格式化函数
- `frontend/src/hooks/useMediaQuery.ts` — 响应式断点检测
- `frontend/src/hooks/useTimeRange.ts` — 时间范围状态管理
- `frontend/src/components/shared/StatCard.tsx` — 统一统计卡片
- `frontend/src/components/shared/TimeRangeSelector.tsx` — 时间区间选择器
- `frontend/src/components/shared/ChartContainer.tsx` — 通用图表容器
- `frontend/src/components/shared/MobileBottomNav.tsx` — 移动端底部导航

### 修改文件
- `frontend/src/components/Navbar.tsx` — 集成 useMediaQuery，移动端隐藏
- `frontend/src/components/Dashboard.tsx` — 使用 ChartContainer、StatCard
- `frontend/src/components/TrafficChart.tsx` — 使用 ChartContainer、useTimeRange
- `frontend/src/components/ClientList.tsx` — 移动端卡片布局
- `frontend/src/components/ClientDetail.tsx` — 移动端全屏、ChartContainer
- `frontend/src/components/QualityPage.tsx` — ChartContainer、响应式布局
- `frontend/src/components/ShadowsocksPage.tsx` — ChartContainer、useTimeRange
- `frontend/src/components/TrojanPage.tsx` — ChartContainer、useTimeRange
- `frontend/src/components/LogsPage.tsx` — 移动端高度适配

---

### Task 1: 创建共享格式化函数

**Files:**
- Create: `frontend/src/utils/format.ts`

- [ ] **Step 1: 创建 format.ts**

```typescript
// frontend/src/utils/format.ts

export const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

export const formatBps = (bytesPerSec: number): string =>
  formatBytes(bytesPerSec) + '/s';

export const formatMs = (value: number): string => {
  if (value < 10) return value.toFixed(1) + ' ms';
  if (value < 100) return value.toFixed(0) + ' ms';
  return Math.round(value).toString() + ' ms';
};

export const formatPercent = (value: number): string =>
  (value * 100).toFixed(1) + '%';
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/utils/format.ts
git commit -m "feat(frontend): add shared format utility functions"
```

---

### Task 2: 创建 useMediaQuery hook

**Files:**
- Create: `frontend/src/hooks/useMediaQuery.ts`

- [ ] **Step 1: 创建 useMediaQuery.ts**

```typescript
// frontend/src/hooks/useMediaQuery.ts

import { useState, useEffect } from 'react';

export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() => {
    if (typeof window !== 'undefined') {
      return window.matchMedia(query).matches;
    }
    return false;
  });

  useEffect(() => {
    const mql = window.matchMedia(query);
    const handler = (e: MediaQueryListEvent) => setMatches(e.matches);
    mql.addEventListener('change', handler);
    return () => mql.removeEventListener('change', handler);
  }, [query]);

  return matches;
}
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/hooks/useMediaQuery.ts
git commit -m "feat(frontend): add useMediaQuery hook for responsive breakpoints"
```

---

### Task 3: 创建 useTimeRange hook

**Files:**
- Create: `frontend/src/hooks/useTimeRange.ts`

- [ ] **Step 1: 创建 useTimeRange.ts**

```typescript
// frontend/src/hooks/useTimeRange.ts

import { useState, useCallback, useMemo } from 'react';

export type TimePreset = '15m' | '1h' | '6h' | '24h' | '7d' | 'custom';

const PRESET_MS: Record<TimePreset, number | null> = {
  '15m': 15 * 60 * 1000,
  '1h': 60 * 60 * 1000,
  '6h': 6 * 60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
  '7d': 7 * 24 * 60 * 60 * 1000,
  'custom': null,
};

export interface TimeRange {
  preset: TimePreset;
  startMs: number;
  endMs: number;
}

export function useTimeRange(defaultPreset: TimePreset = '1h') {
  const [preset, setPresetState] = useState<TimePreset>(defaultPreset);
  const [customStart, setCustomStart] = useState<number>(Date.now() - 3600000);
  const [customEnd, setCustomEnd] = useState<number>(Date.now());

  const setPreset = useCallback((p: TimePreset) => {
    setPresetState(p);
  }, []);

  const setCustomRange = useCallback((startMs: number, endMs: number) => {
    setCustomStart(startMs);
    setCustomEnd(endMs);
    setPresetState('custom');
  }, []);

  const range: TimeRange = useMemo(() => {
    if (preset === 'custom') {
      return { preset, startMs: customStart, endMs: customEnd };
    }
    const duration = PRESET_MS[preset]!;
    const now = Date.now();
    return { preset, startMs: now - duration, endMs: now };
  }, [preset, customStart, customEnd]);

  const presets: TimePreset[] = ['15m', '1h', '6h', '24h', '7d'];

  return { range, preset, presets, setPreset, setCustomRange };
}
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/hooks/useTimeRange.ts
git commit -m "feat(frontend): add useTimeRange hook for chart time range management"
```

---

### Task 4: 创建 StatCard 组件

**Files:**
- Create: `frontend/src/components/shared/StatCard.tsx`

- [ ] **Step 1: 创建 StatCard.tsx**

```typescript
// frontend/src/components/shared/StatCard.tsx

import React from 'react';

interface StatCardProps {
  label: string;
  value: string;
  icon: React.ReactNode;
  color?: 'blue' | 'green' | 'purple' | 'orange' | 'yellow' | 'red';
  valueColor?: string;
}

const colorClasses: Record<string, { bg: string; text: string }> = {
  blue: { bg: 'bg-blue-500', text: 'text-blue-600' },
  green: { bg: 'bg-green-500', text: 'text-green-600' },
  purple: { bg: 'bg-purple-500', text: 'text-purple-600' },
  orange: { bg: 'bg-orange-500', text: 'text-orange-600' },
  yellow: { bg: 'bg-yellow-500', text: 'text-yellow-600' },
  red: { bg: 'bg-red-500', text: 'text-red-600' },
};

export const StatCard = ({ label, value, icon, color = 'blue', valueColor }: StatCardProps) => {
  const c = colorClasses[color];
  return (
    <div className="bg-white overflow-hidden shadow rounded-lg p-4 sm:p-6">
      <div className="flex items-center">
        <div className={`flex-shrink-0 ${c.bg} rounded-md p-3`}>
          {icon}
        </div>
        <div className="ml-5 w-0 flex-1">
          <dl>
            <dt className="text-sm font-medium text-gray-500 truncate">{label}</dt>
            <dd className={`text-lg font-semibold ${valueColor || 'text-gray-900'}`}>{value}</dd>
          </dl>
        </div>
      </div>
    </div>
  );
};
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/shared/StatCard.tsx
git commit -m "feat(frontend): add StatCard shared component"
```

---

### Task 5: 创建 TimeRangeSelector 组件

**Files:**
- Create: `frontend/src/components/shared/TimeRangeSelector.tsx`

- [ ] **Step 1: 创建 TimeRangeSelector.tsx**

```typescript
// frontend/src/components/shared/TimeRangeSelector.tsx

import React from 'react';
import { useMediaQuery } from '../../hooks/useMediaQuery';
import type { TimePreset } from '../../hooks/useTimeRange';

interface TimeRangeSelectorProps {
  preset: TimePreset;
  presets: TimePreset[];
  customStartMs: number;
  customEndMs: number;
  onPresetChange: (preset: TimePreset) => void;
  onCustomChange: (startMs: number, endMs: number) => void;
}

const PRESET_LABELS: Record<string, string> = {
  '15m': '15min',
  '1h': '1h',
  '6h': '6h',
  '24h': '24h',
  '7d': '7d',
};

const toDatetimeLocal = (ms: number): string => {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

export const TimeRangeSelector = ({
  preset,
  presets,
  customStartMs,
  customEndMs,
  onPresetChange,
  onCustomChange,
}: TimeRangeSelectorProps) => {
  const isMobile = useMediaQuery('(max-width: 639px)');

  return (
    <div className={`flex items-center gap-2 ${isMobile ? 'flex-col items-start' : 'flex-wrap'}`}>
      <div className="flex rounded-md shadow-sm" role="group">
        {presets.map((p) => (
          <button
            key={p}
            type="button"
            onClick={() => onPresetChange(p)}
            className={`px-3 py-1.5 text-xs font-medium border transition-colors
              first:rounded-l-md last:rounded-r-md
              ${preset === p
                ? 'bg-blue-600 text-white border-blue-600 z-10'
                : 'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
              }`}
          >
            {PRESET_LABELS[p] || p}
          </button>
        ))}
      </div>

      <div className={`flex items-center gap-1 ${isMobile ? 'flex-col items-start w-full' : ''}`}>
        <button
          type="button"
          onClick={() => onPresetChange('custom')}
          className={`px-3 py-1.5 text-xs font-medium rounded-md border transition-colors
            ${preset === 'custom'
              ? 'bg-blue-600 text-white border-blue-600'
              : 'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
            }`}
        >
          Custom
        </button>
        {preset === 'custom' && (
          <div className={`flex items-center gap-1 ${isMobile ? 'flex-col w-full' : ''}`}>
            <input
              type="datetime-local"
              value={toDatetimeLocal(customStartMs)}
              onChange={(e) => {
                const v = new Date(e.target.value).getTime();
                if (!isNaN(v)) onCustomChange(v, customEndMs);
              }}
              className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full sm:w-auto"
            />
            <span className="text-xs text-gray-400">-</span>
            <input
              type="datetime-local"
              value={toDatetimeLocal(customEndMs)}
              onChange={(e) => {
                const v = new Date(e.target.value).getTime();
                if (!isNaN(v)) onCustomChange(customStartMs, v);
              }}
              className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full sm:w-auto"
            />
          </div>
        )}
      </div>
    </div>
  );
};
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/shared/TimeRangeSelector.tsx
git commit -m "feat(frontend): add TimeRangeSelector shared component"
```

---

### Task 6: 创建 ChartContainer 组件

**Files:**
- Create: `frontend/src/components/shared/ChartContainer.tsx`

- [ ] **Step 1: 创建 ChartContainer.tsx**

```typescript
// frontend/src/components/shared/ChartContainer.tsx

import React, { useState, useCallback } from 'react';
import { TimeRangeSelector } from './TimeRangeSelector';
import type { TimePreset } from '../../hooks/useTimeRange';

export interface ChartTimeRange {
  preset: TimePreset;
  startMs: number;
  endMs: number;
}

interface ChartContainerProps {
  title: string;
  timeRange?: ChartTimeRange;
  presets?: TimePreset[];
  onTimeRangeChange?: (range: ChartTimeRange) => void;
  loading?: boolean;
  isEmpty?: boolean;
  children: React.ReactNode;
  className?: string;
}

export const ChartContainer = ({
  title,
  timeRange,
  presets,
  onTimeRangeChange,
  loading = false,
  isEmpty = false,
  children,
  className = '',
}: ChartContainerProps) => {
  const [customStart, setCustomStart] = useState(() => Date.now() - 3600000);
  const [customEnd, setCustomEnd] = useState(() => Date.now());

  const handlePresetChange = useCallback((p: TimePreset) => {
    if (!onTimeRangeChange) return;
    setCustomStart(Date.now() - 3600000); // reset
    setCustomEnd(Date.now());
    if (p === 'custom') {
      onTimeRangeChange({ preset: 'custom', startMs: customStart, endMs: customEnd });
    } else {
      const map: Record<string, number> = {
        '15m': 15 * 60 * 1000,
        '1h': 60 * 60 * 1000,
        '6h': 6 * 60 * 60 * 1000,
        '24h': 24 * 60 * 60 * 1000,
        '7d': 7 * 24 * 60 * 60 * 1000,
      };
      const dur = map[p] || 3600000;
      const now = Date.now();
      onTimeRangeChange({ preset: p, startMs: now - dur, endMs: now });
    }
  }, [onTimeRangeChange, customStart, customEnd]);

  const handleCustomChange = useCallback((startMs: number, endMs: number) => {
    setCustomStart(startMs);
    setCustomEnd(endMs);
    if (onTimeRangeChange) {
      onTimeRangeChange({ preset: 'custom', startMs, endMs });
    }
  }, [onTimeRangeChange]);

  const defaultPresets: TimePreset[] = ['15m', '1h', '6h', '24h', '7d'];

  return (
    <div className={`bg-white p-4 sm:p-6 rounded-lg shadow ${className}`}>
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between mb-4 gap-3">
        <h3 className="text-lg font-medium text-gray-900">{title}</h3>
        {timeRange && onTimeRangeChange && (
          <TimeRangeSelector
            preset={timeRange.preset}
            presets={presets || defaultPresets}
            customStartMs={customStart}
            customEndMs={customEnd}
            onPresetChange={handlePresetChange}
            onCustomChange={handleCustomChange}
          />
        )}
      </div>
      {loading ? (
        <div className="flex items-center justify-center py-8">
          <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600" />
        </div>
      ) : isEmpty ? (
        <p className="text-gray-500 text-center py-8">No data available</p>
      ) : (
        children
      )}
    </div>
  );
};
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/shared/ChartContainer.tsx
git commit -m "feat(frontend): add ChartContainer shared component with time range"
```

---

### Task 7: 创建 MobileBottomNav 组件

**Files:**
- Create: `frontend/src/components/shared/MobileBottomNav.tsx`

- [ ] **Step 1: 创建 MobileBottomNav.tsx**

```typescript
// frontend/src/components/shared/MobileBottomNav.tsx

import React from 'react';

type Tab = 'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs';

interface MobileBottomNavProps {
  activeTab: Tab;
  onTabChange: (tab: Tab) => void;
}

const tabs: { id: Tab; label: string; icon: React.ReactNode }[] = [
  {
    id: 'dashboard',
    label: 'Home',
    icon: <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M3 12l2-2m0 0l7-7 7 7M5 10v10a1 1 0 001 1h3m10-11l2 2m-2-2v10a1 1 0 01-1 1h-3m-4 0a1 1 0 01-1-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 01-1 1" />,
  },
  {
    id: 'quality',
    label: 'Quality',
    icon: <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 10V3L4 14h7v7l9-11h-7z" />,
  },
  {
    id: 'shadowsocks',
    label: 'SS',
    icon: <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />,
  },
  {
    id: 'trojan',
    label: 'Trojan',
    icon: <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />,
  },
  {
    id: 'logs',
    label: 'Logs',
    icon: <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 6h16M4 10h16M4 14h16M4 18h16" />,
  },
];

export const MobileBottomNav = ({ activeTab, onTabChange }: MobileBottomNavProps) => {
  return (
    <nav
      className="md:hidden fixed bottom-0 left-0 right-0 bg-white border-t border-gray-200 z-40"
      style={{ paddingBottom: 'env(safe-area-inset-bottom, 0px)' }}
    >
      <div className="flex justify-around items-center h-14">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => onTabChange(tab.id)}
            className={`flex flex-col items-center justify-center min-w-0 px-1 py-1 transition-colors
              ${activeTab === tab.id ? 'text-blue-600' : 'text-gray-400 hover:text-gray-600'}`}
          >
            <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              {tab.icon}
            </svg>
            <span className="text-[10px] mt-0.5 font-medium truncate">{tab.label}</span>
          </button>
        ))}
      </div>
    </nav>
  );
};
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/shared/MobileBottomNav.tsx
git commit -m "feat(frontend): add MobileBottomNav component"
```

---

### Task 8: 重构 TrafficChart

**Files:**
- Modify: `frontend/src/components/TrafficChart.tsx`

- [ ] **Step 1: 更新 TrafficChart.tsx 使用 ChartContainer 和时间筛选**

```typescript
// frontend/src/components/TrafficChart.tsx

import { useState, useMemo, useCallback } from 'react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import type { PortTraffic } from '../types';
import { ChartContainer } from './shared/ChartContainer';
import type { ChartTimeRange } from './shared/ChartContainer';
import { formatBytes } from '../utils/format';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  const [timeRange, setTimeRange] = useState<ChartTimeRange>({
    preset: '1h',
    startMs: Date.now() - 3600000,
    endMs: Date.now(),
  });

  const handleTimeRangeChange = useCallback((range: ChartTimeRange) => {
    setTimeRange(range);
  }, []);

  const chartData = useMemo(() => {
    const timeMap = new Map<number, Record<string, number | string>>();

    for (const portTraffic of traffic) {
      for (const bucket of portTraffic.buckets) {
        const ts = new Date(bucket.timestamp).getTime();
        // Filter by time range
        if (ts < timeRange.startMs || ts > timeRange.endMs) continue;
        if (!timeMap.has(ts)) {
          timeMap.set(ts, { time: ts });
        }
        const point = timeMap.get(ts)!;
        point[`in_${portTraffic.port}`] = bucket.bytes_in;
        point[`out_${portTraffic.port}`] = bucket.bytes_out;
      }
    }

    return Array.from(timeMap.values())
      .sort((a, b) => (a.time as number) - (b.time as number));
  }, [traffic, timeRange]);

  const colorPool = ['#3b82f6', '#10b981', '#8b5cf6', '#f59e0b', '#ef4444', '#06b6d4'];

  return (
    <ChartContainer
      title="Network Traffic"
      timeRange={timeRange}
      onTimeRangeChange={handleTimeRangeChange}
      isEmpty={chartData.length === 0}
    >
      <ResponsiveContainer width="100%" height={window.innerWidth < 640 ? 250 : 300}>
        <LineChart data={chartData}>
          <CartesianGrid strokeDasharray="3 3" />
          <XAxis
            dataKey="time"
            tick={{ fontSize: window.innerWidth < 640 ? 9 : 12 }}
            tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()}
          />
          <YAxis
            tick={{ fontSize: window.innerWidth < 640 ? 9 : 12 }}
            tickFormatter={formatBytes}
            width={70}
          />
          <Tooltip
            formatter={(value: number) => formatBytes(value)}
            labelFormatter={(ts: number) => new Date(ts).toLocaleString()}
          />
          <Legend
            wrapperStyle={{ fontSize: window.innerWidth < 640 ? '10px' : '12px' }}
          />
          {traffic.map((portTraffic, idx) => (
            <>
              <Line
                key={`in-${portTraffic.port}`}
                type="monotone"
                dataKey={`in_${portTraffic.port}`}
                name={`In (Port ${portTraffic.port})`}
                stroke={colorPool[idx * 2 % colorPool.length]}
                dot={false}
                strokeWidth={2}
              />
              <Line
                key={`out-${portTraffic.port}`}
                type="monotone"
                dataKey={`out_${portTraffic.port}`}
                name={`Out (Port ${portTraffic.port})`}
                stroke={colorPool[(idx * 2 + 1) % colorPool.length]}
                dot={false}
                strokeWidth={2}
              />
            </>
          ))}
        </LineChart>
      </ResponsiveContainer>
    </ChartContainer>
  );
};
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/TrafficChart.tsx
git commit -m "feat(frontend): refactor TrafficChart with time range filtering and ChartContainer"
```

---

### Task 9: 重构 Dashboard

**Files:**
- Modify: `frontend/src/components/Dashboard.tsx`

- [ ] **Step 1: 更新 Dashboard.tsx 使用 StatCard**

```typescript
// frontend/src/components/Dashboard.tsx

import { useState } from 'react';
import { useQuery } from 'react-query';
import { Navbar } from './Navbar';
import { ClientList } from './ClientList';
import { TrafficChart } from './TrafficChart';
import { ClientDetail } from './ClientDetail';
import { QualityPage } from './QualityPage';
import { ShadowsocksPage } from './ShadowsocksPage';
import { TrojanPage } from './TrojanPage';
import { LogsPage } from './LogsPage';
import { MobileBottomNav } from './shared/MobileBottomNav';
import { StatCard } from './shared/StatCard';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { getMetrics, getTraffic } from '../api/client';
import { formatBytes } from '../utils/format';

interface DashboardProps {
  onLogout: () => void;
}

export const Dashboard = ({ onLogout }: DashboardProps) => {
  const [selectedPort, setSelectedPort] = useState<number | null>(null);
  const [activeTab, setActiveTab] = useState<'dashboard' | 'quality' | 'shadowsocks' | 'trojan' | 'logs'>('dashboard');
  const isMobile = useMediaQuery('(max-width: 767px)');

  const { data: metrics } = useQuery('metrics', getMetrics, {
    refetchInterval: 5000,
  });

  const { data: traffic = [] } = useQuery('traffic', getTraffic, {
    refetchInterval: 5000,
  });

  const handleSelectClient = (port: number) => {
    setSelectedPort(port);
  };

  return (
    <div className="min-h-screen bg-gray-100">
      <Navbar onLogout={onLogout} activeTab={activeTab} onTabChange={setActiveTab} />
      <main className={`max-w-7xl mx-auto py-6 px-4 sm:px-6 lg:px-8 ${isMobile ? 'pb-20' : ''}`}>
        {activeTab === 'dashboard' ? (
          <>
            <div className="grid grid-cols-2 gap-3 sm:gap-5 sm:grid-cols-2 lg:grid-cols-4 mb-6">
              <StatCard
                label="Connected Clients"
                value={String(metrics?.client_count || 0)}
                color="blue"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
                  </svg>
                }
              />
              <StatCard
                label="Active Connections"
                value={String(metrics?.active_connection_count || 0)}
                color="green"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 7h12m0 0l-4-4m4 4l-4 4m0 6H4m0 0l4 4m-4-4l4-4" />
                  </svg>
                }
              />
              <StatCard
                label="Total Bytes In"
                value={formatBytes(metrics?.total_bytes_in || 0)}
                color="purple"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M7 16l-4-4m0 0l4-4m-4 4h18" />
                  </svg>
                }
              />
              <StatCard
                label="Total Bytes Out"
                value={formatBytes(metrics?.total_bytes_out || 0)}
                color="orange"
                icon={
                  <svg className="h-6 w-6 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M17 8l4 4m0 0l-4 4m4-4H3" />
                  </svg>
                }
              />
            </div>

            <div className="space-y-6">
              <ClientList onSelectClient={handleSelectClient} />
              <TrafficChart traffic={traffic} />
            </div>
          </>
        ) : activeTab === 'quality' ? (
          <QualityPage onSelectClient={handleSelectClient} />
        ) : activeTab === 'shadowsocks' ? (
          <ShadowsocksPage />
        ) : activeTab === 'trojan' ? (
          <TrojanPage />
        ) : (
          <LogsPage />
        )}
      </main>

      {isMobile && <MobileBottomNav activeTab={activeTab} onTabChange={setActiveTab} />}

      {selectedPort !== null && (
        <ClientDetail
          port={selectedPort}
          onClose={() => setSelectedPort(null)}
        />
      )}
    </div>
  );
};
```

- [ ] **Step 2: 删除 Dashboard 中不再需要的 `formatBytes` 函数**

Dashboard 中原有的 `formatBytes` 函数需要删除（已在 import 中引入 from utils/format）。

- [ ] **Step 3: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 4: 提交**

```bash
git add frontend/src/components/Dashboard.tsx
git commit -m "feat(frontend): refactor Dashboard with StatCard and MobileBottomNav"
```

---

### Task 10: 重构 Navbar（集成 useMediaQuery 隐藏移动端）

**Files:**
- Modify: `frontend/src/components/Navbar.tsx`

- [ ] **Step 1: 更新 Navbar.tsx 添加 `hidden md:flex` 到导航链接**

在导航链接的容器 div 上添加 `hidden md:flex` class，使其在移动端隐藏：

```typescript
// frontend/src/components/Navbar.tsx
// 仅修改一行：第 29 行的 <div className="ml-10 flex space-x-4">
// 改为：

<div className="hidden md:flex ml-10 space-x-4">
```

同时给整个 nav 添加 `md:block` 确保在移动端也能显示标题栏：

```typescript
<nav className="bg-gray-800">
  <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
    <div className="flex items-center justify-between h-16">
      <div className="flex items-center">
        <div className="flex-shrink-0">
          <h1 className="text-white text-xl font-bold">Rust Tunnel</h1>
        </div>
        <div className="hidden md:flex ml-10 space-x-4">
          {/* ... existing tab buttons ... */}
        </div>
      </div>
```

注意：完整的 Navbar.tsx 保持原有结构，只需要在导航链接容器和按钮上添加 `hidden md:flex` 前缀即可。在移动端标题仍然显示。

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/Navbar.tsx
git commit -m "feat(frontend): hide Navbar tabs on mobile, delegate to MobileBottomNav"
```

---

### Task 11: 重构 ClientList（移动端卡片布局）

**Files:**
- Modify: `frontend/src/components/ClientList.tsx`

- [ ] **Step 1: 替换 formatRtt/formatLossRate，添加 useMediaQuery，实现移动端卡片**

```typescript
// frontend/src/components/ClientList.tsx
// 在文件顶部添加 import:

import { useMediaQuery } from '../hooks/useMediaQuery';
import { formatMs, formatPercent } from '../utils/format';

// 删除原有的 formatRtt 和 formatLossRate 函数（第 22-31 行）

// 在 ClientList 组件内部，在 queryClient 之后添加：
const isSmallScreen = useMediaQuery('(max-width: 639px)');

// 将表格行提取为可复用组件（在 ClientList 组件上方定义）:
const ClientCard = ({ client, onSelectClient, onDisconnect, disabled }: {
  client: ClientResponse;
  onSelectClient?: (port: number) => void;
  onDisconnect: (port: number) => void;
  disabled: boolean;
}) => (
  <div className="bg-gray-50 border border-gray-200 rounded-lg p-4">
    <div className="flex items-center justify-between mb-2">
      <span className="text-sm font-semibold text-gray-900">Port {client.port}</span>
      <QualityIndicator quality={client.quality} />
    </div>
    <div className="grid grid-cols-2 gap-2 text-xs text-gray-500 mb-3">
      <span>RTT: {client.quality ? formatMs(client.quality.avg_rtt_ms) : 'N/A'}</span>
      <span>Loss: {client.quality ? formatPercent(client.quality.loss_rate) : 'N/A'}</span>
      <span>Connections: {client.connection_count}</span>
    </div>
    <div className="flex justify-end space-x-3">
      <button
        onClick={() => onSelectClient?.(client.port)}
        className="text-blue-600 hover:text-blue-900 text-sm font-medium"
      >
        Details
      </button>
      <button
        onClick={() => onDisconnect(client.port)}
        disabled={disabled}
        className="text-red-600 hover:text-red-900 text-sm font-medium disabled:opacity-50"
      >
        Disconnect
      </button>
    </div>
  </div>
);
```

然后在渲染部分，用条件判断替换表格：

```tsx
{/* 移动端：卡片布局 */}
{isSmallScreen ? (
  <div className="space-y-3">
    {group.clients.map((client) => (
      <ClientCard
        key={client.port}
        client={client}
        onSelectClient={onSelectClient}
        onDisconnect={handleDisconnect}
        disabled={disconnectMutation.isLoading}
      />
    ))}
  </div>
) : (
  /* 桌面端：保持原有表格布局（不变） */
  <div className="overflow-x-auto">
    <table className="min-w-full divide-y divide-gray-200">
      {/* ...原有表格代码完全不变... */}
    </table>
  </div>
)}
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/ClientList.tsx
git commit -m "feat(frontend): add mobile card layout for ClientList"
```

---

### Task 12: 重构 QualityPage

**Files:**
- Modify: `frontend/src/components/QualityPage.tsx`

- [ ] **Step 1: 更新 QualityPage - 替换 StatCard、添加时间筛选、响应式优化**

主要改动：
1. 将 `QualityMetrics` 中的 4 个卡片替换为 `StatCard` 组件
2. 将 `formatMs` 和 `formatPercent` 替换为来自 `utils/format` 的导入
3. 热力图网格改为 `grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4`
4. Worst Connections 表格添加移动端卡片适配

```typescript
// frontend/src/components/QualityPage.tsx 顶部添加:
import { StatCard } from './shared/StatCard';
import { formatMs, formatPercent } from '../utils/format';
import { useMediaQuery } from '../hooks/useMediaQuery';

// 删除原有的 formatMs 和 formatPercent 函数（第 6-8 行）

// QualityMetrics 中的 StatCard:
// 替换 4 个原有卡片。以 Avg Quality Score 为例：
<StatCard
  label="Avg Quality Score"
  value={`${avgScore} (${getQualityText(avgScore)})`}
  color="blue"
  valueColor={getQualityColor(avgScore)}
  icon={
    <svg className="h-6 w-6" style={{ color: getQualityColor(avgScore) }} fill="none" viewBox="0 0 24 24" stroke="currentColor">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 10V3L4 14h7v7l9-11h-7z" />
    </svg>
  }
/>
```

替换其余 3 个 StatCard（Clients Monitored=blue, Warnings=yellow, Critical=red）。

Worst Connections 表格添加移动端适配：
```typescript
const isSmallScreen = useMediaQuery('(max-width: 639px)');
// 在 WorstConnectionsTable 中，isSmallScreen 为 true 时渲染卡片而非表格
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/QualityPage.tsx
git commit -m "feat(frontend): refactor QualityPage with StatCard and responsive layout"
```

---

### Task 13: 重构 ShadowsocksPage

**Files:**
- Modify: `frontend/src/components/ShadowsocksPage.tsx`

- [ ] **Step 1: 使用 ChartContainer、format 共享函数、添加时间筛选**

主要改动：
1. 删除本地 `formatBytes`、`formatBps` 函数，改用 `utils/format`
2. 添加 `useTimeRange` hook 管理 ThroughputHistory 的时间范围
3. ThroughputHistory 接收 `timeRange` prop 并按时间过滤数据
4. 将 ThroughputHistory 的卡片样式改为 `ChartContainer`

```typescript
// frontend/src/components/ShadowsocksPage.tsx 顶部:
import { useState, useMemo, useCallback } from 'react';
import { ChartContainer } from './shared/ChartContainer';
import type { ChartTimeRange } from './shared/ChartContainer';
import { formatBytes, formatBps } from '../utils/format';

// 删除本地 formatBytes、formatBps 函数（第 7-15 行）

// ThroughputHistory 组件添加 timeRange prop:
const ThroughputHistory = ({ qualityList, timeRange }: {
  qualityList: ShadowsocksQuality[];
  timeRange: ChartTimeRange;
}) => {

// 在数据合并时添加时间过滤:
if (ts < timeRange.startMs || ts > timeRange.endMs) continue;

// 在 ShadowsocksPage 主组件中:
const [timeRange, setTimeRange] = useState<ChartTimeRange>({
  preset: '1h',
  startMs: Date.now() - 3600000,
  endMs: Date.now(),
});

// 将 ThroughputHistory 用 ChartContainer 包裹:
<ChartContainer
  title="Throughput History"
  timeRange={timeRange}
  onTimeRangeChange={setTimeRange}
  isEmpty={qualityList.every(q => q.history.length === 0)}
>
  <ThroughputHistory qualityList={qualityList} timeRange={timeRange} />
</ChartContainer>
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/ShadowsocksPage.tsx
git commit -m "feat(frontend): refactor ShadowsocksPage with ChartContainer and time range"
```

---

### Task 14: 重构 TrojanPage

**Files:**
- Modify: `frontend/src/components/TrojanPage.tsx`

与 Task 13（ShadowsocksPage）改动完全对称，处理方式完全相同。

主要改动：
1. 删除本地 `formatBytes`、`formatBps`
2. 导入 `ChartContainer`、`ChartTimeRange`、`formatBytes`、`formatBps`
3. ThroughputHistory 添加 `timeRange` 参数和时间过滤
4. 用 `ChartContainer` 包裹 ThroughputHistory 区域

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/TrojanPage.tsx
git commit -m "feat(frontend): refactor TrojanPage with ChartContainer and time range"
```

---

### Task 15: 重构 ClientDetail

**Files:**
- Modify: `frontend/src/components/ClientDetail.tsx`

- [ ] **Step 1: 移动端全屏 + 使用 format 共享函数**

主要改动：
1. 删除本地 `formatBytes`、`formatMs`、`formatPercent`，改用 `utils/format`
2. 导入 `useMediaQuery`
3. 移动端弹窗变为全屏：
   - `< 640px`：`fixed inset-0 rounded-none max-w-full max-h-full`
   - `>= 640px`：保持原来的 `max-w-2xl rounded-lg max-h-[90vh]`

```typescript
// frontend/src/components/ClientDetail.tsx 顶部:
import { useMediaQuery } from '../hooks/useMediaQuery';
import { formatBytes, formatMs, formatPercent } from '../utils/format';

// 删除本地 formatBytes, formatMs, formatPercent (第 13-23 行)

// 在组件内:
const isSmallScreen = useMediaQuery('(max-width: 639px)');

// 弹窗外层 div:
<div className={`fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center p-4 z-50`}>
  <div className={`bg-white shadow-xl w-full overflow-hidden
    ${isSmallScreen
      ? 'rounded-none max-w-full h-full'
      : 'rounded-lg max-w-2xl max-h-[90vh]'
    }`}>
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/ClientDetail.tsx
git commit -m "feat(frontend): refactor ClientDetail with fullscreen on mobile and shared format utils"
```

---

### Task 16: 重构 LogsPage（移动端高度适配）

**Files:**
- Modify: `frontend/src/components/LogsPage.tsx`

- [ ] **Step 1: 移动端日志区高度适配 + 底部间距**

主要改动：
1. 导入 `useMediaQuery`
2. 日志显示区高度根据是否为移动端动态调整
3. 确保移动端底部导航不遮挡日志内容

```typescript
// frontend/src/components/LogsPage.tsx 顶部添加:
import { useMediaQuery } from '../hooks/useMediaQuery';

// 在组件内:
const isMobile = useMediaQuery('(max-width: 767px)');

// 日志显示区 style:
style={{ height: isMobile ? 'calc(100vh - 280px)' : '600px' }}
```

- [ ] **Step 2: 验证构建**

Run: `cd frontend && npm run build`
Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add frontend/src/components/LogsPage.tsx
git commit -m "feat(frontend): adapt LogsPage height for mobile layout"
```

---

## 最终验证

完成所有 Task 后，运行完整构建和验证：

```bash
cd frontend && npm run build
```

确保 TypeScript 编译通过且 Vite 构建成功。
