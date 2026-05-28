# 前端图表优化与移动端适配 - 设计文档

**日期：** 2026-05-28
**状态：** 已批准
**方案：** 方案二（提取共享组件 + 适度重构）

---

## 目标

1. 所有图表支持时间区间选择（预设 + 自定义范围）
2. 移动端响应式布局，底部标签栏导航
3. 图表交互增强（图例切换、tooltip 详情等）
4. 整体视觉优化

---

## 共享模块架构

```
frontend/src/
├── components/
│   ├── shared/
│   │   ├── TimeRangeSelector.tsx   # 时间区间选择器
│   │   ├── ChartContainer.tsx      # 通用图表容器
│   │   ├── MobileBottomNav.tsx     # 移动端底部标签栏
│   │   └── StatCard.tsx            # 统计卡片
├── hooks/
│   ├── useTimeRange.ts             # 时间范围状态管理
│   └── useMediaQuery.ts            # 响应式断点检测
├── utils/
│   └── format.ts                   # 共享格式化函数
```

---

## 组件详细设计

### TimeRangeSelector

- 预设按钮：15分钟、1小时、6小时、24小时、7天
- 自定义范围：两个 `datetime-local` 输入（开始/结束）
- 选中预设时高亮，选择自定义时清除预设高亮
- 接口：`onChange(startMs: number, endMs: number)`

### ChartContainer

统一包装所有图表，替代分散的卡片 + 标题代码：

```tsx
<ChartContainer
  title="Network Traffic"
  timeRange={timeRange}
  onTimeRangeChange={setTimeRange}
  loading={isLoading}
  isEmpty={data.length === 0}
>
  <LineChart>...</LineChart>
</ChartContainer>
```

### MobileBottomNav

- 在 `< 768px` 时显示，固定在屏幕底部
- 5 个标签：Dashboard、Quality、Shadowsocks、Trojan、Logs
- 使用 SVG 图标 + 文字，激活项高亮
- 适配 `safe-area-inset-bottom`

### StatCard

统一统计卡片布局，支持自定义图标和颜色：

```tsx
<StatCard icon={<svg>...</svg>} color="blue" label="Total Bytes In" value={formatBytes(1024)} />
```

### useTimeRange

```tsx
const { startMs, endMs, preset, setPreset, setCustomRange } = useTimeRange(defaultPreset);
```

管理时间范围状态，预设与自定义互斥。

### useMediaQuery

```tsx
const isMobile = useMediaQuery('(max-width: 767px)');
const isSmallScreen = useMediaQuery('(max-width: 639px)');
```

### format.ts

从各组件中提取的共享函数：
- `formatBytes` - 字节格式化
- `formatBps` - 速率格式化
- `formatMs` - 毫秒格式化
- `formatPercent` - 百分比格式化

---

## 各页面响应式改造

### Dashboard

- 指标卡片：保持 `grid-cols-1 sm:grid-cols-2 lg:grid-cols-4`
- TrafficChart 使用 ChartContainer，自带时间选择器
- 移动端时间选择器折叠到图表下方

### ClientList

- 桌面端（>= 640px）：保持表格布局
- 移动端（< 640px）：每条客户端一张卡片
  - Port、质量指示器、RTT、Loss、Connections
  - Details / Disconnect 按钮

### QualityPage

- 指标卡片：桌面 4 列 → 手机 2 列
- 热力图：桌面 3-4 列 → 手机 1-2 列
- Worst Connections 表格：桌面表格 → 手机卡片列表

### ShadowsocksPage / TrojanPage

- 配置区域：桌面 3 列 → 手机 1 列
- 统计区域：桌面 4 列 → 手机 2 列
- 吞吐量图表使用 ChartContainer

### ClientDetail

- 桌面端：模态弹窗
- 移动端（< 640px）：全屏显示

### LogsPage

- 控制栏优化间距（flex-wrap 已有）
- 日志区高度：桌面 600px → 手机 `calc(100vh - 300px)`
- 底部导航栏留出额外边距

---

## 图表交互增强

- Recharts Tooltip 自定义内容：当前值、平均值
- 图例点击切换显示/隐藏对应线条（Recharts 原生支持）
- 移动端图表高度：桌面 300px → 手机 250px
- X 轴 tick 数量自适应屏幕宽度

---

## 后端 API 考虑

当前后端 API 返回所有历史数据，前端做筛选。如果数据量大，后续可按需添加 `start`/`end` 查询参数。本次实现以前端筛选为主，不阻塞后端改动。

---

## 不包含的内容

- 不更换图表库（保持 Recharts）
- 不引入暗色模式（可作为后续优化）
- 不修改后端 API
- 不添加国际化

---

## 文件变更清单

### 新增文件
- `frontend/src/components/shared/TimeRangeSelector.tsx`
- `frontend/src/components/shared/ChartContainer.tsx`
- `frontend/src/components/shared/MobileBottomNav.tsx`
- `frontend/src/components/shared/StatCard.tsx`
- `frontend/src/hooks/useTimeRange.ts`
- `frontend/src/hooks/useMediaQuery.ts`
- `frontend/src/utils/format.ts`

### 修改文件
- `frontend/src/components/Dashboard.tsx`
- `frontend/src/components/Navbar.tsx`
- `frontend/src/components/TrafficChart.tsx`
- `frontend/src/components/ClientList.tsx`
- `frontend/src/components/ClientDetail.tsx`
- `frontend/src/components/QualityPage.tsx`
- `frontend/src/components/ShadowsocksPage.tsx`
- `frontend/src/components/TrojanPage.tsx`
- `frontend/src/components/LogsPage.tsx`

### 不修改
- `frontend/src/App.tsx`
- `frontend/src/main.tsx`
- `frontend/src/api/client.ts`
- `frontend/src/types/index.ts`
- `frontend/src/index.css`
