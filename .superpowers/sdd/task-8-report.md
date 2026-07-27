# Task 8 Report: 共享组件与残余文案兜底 + 全量验收

## 扫描结果分类

### Scan 1: JSX 内容（`>text<`）

| 文件 | 原文 | 分类 | 处理 |
|---|---|---|---|
| `src/components/ui/dialog.tsx:47` | `Close` (sr-only) | 应迁移 | 已迁移到 `common.close` |
| `src/components/ui/sheet.tsx:68` | `Close` (sr-only) | 应迁移 | 已迁移到 `common.close` |
| `src/components/shadowsocks/ShadowsocksConfigCard.tsx:86` | `AES-256-GCM` | 协议名 → 保留 | — |
| `src/components/reverse-proxy/BackendFields.tsx:140-141` | `http`/`https` | 协议值 → 保留 | — |
| `src/components/reverse-proxy/BackendFields.tsx:157-158` | `http1`/`http2` | 协议值 → 保留 | — |
| `src/pages/DnsPage.tsx:109-110` | `AAAA`/`CNAME` | 技术值 → 保留 | — |

### Scan 2: aria-label/placeholder/title 属性

所有匹配项均为示例域名、协议占位符等教学性值，**全部保留**。

### 已知待处理项

| 项 | 处理 |
|---|---|
| `UsageTab.tsx` `' (stream)'` | 已迁移到 `llm.usage.table.stream` |
| `dashboard.clientList.online/offline` 重复 | DashboardPage 改用 `common.status.*`，locale 中删除重复 key |
| `ClientList.tsx` `getQualityColor`/`getQualityText` | 死代码，已删除（YAGNI） |
| `TimeRangeSelector` Custom 按钮 | 已迁移到 `timeRange.custom` |
| `ThemeToggle` aria-label + 菜单选项 | 已迁移到 `theme.*` 区块（ariaLabel/system/light/dark/systemDesc） |
| `ThemeToggle` 中文硬编码 | 已全部替换为翻译 key |
| `ChartEmpty` 默认文案 | 已迁移：`No data available` → `common.noData`，`Loading...` → `common.loading` |
| `MetricAreaChart` 默认 `emptyText` | 已移除硬编码默认值，由调用方传入翻译文案 |
| `ui/dialog.tsx` `ui/sheet.tsx` sr-only `Close` | 已迁移到 `common.close` |

### 新增 locale key

- `common.noData` — en: "No data available", zh-CN: "暂无数据"
- `common.close` — en: "Close", zh-CN: "关闭"
- `timeRange.custom` — en: "Custom", zh-CN: "自定义"
- `theme.ariaLabel` — en: "Switch theme", zh-CN: "切换主题"
- `theme.system` — en: "System", zh-CN: "跟随系统"
- `theme.light` — en: "Light", zh-CN: "浅色"
- `theme.dark` — en: "Dark", zh-CN: "深色"
- `theme.systemDesc` — en: "Automatically switch based on system theme", zh-CN: "根据系统主题自动切换"
- `llm.usage.table.stream` — en: "stream", zh-CN: "stream"

### 删除的 locale key

- `dashboard.clientList.online` → 统一使用 `common.status.online`
- `dashboard.clientList.offline` → 统一使用 `common.status.offline`

### 验证结果

| 检查项 | 状态 |
|---|---|
| `npx tsc --noEmit` | 通过 |
| `npm run lint` | 通过（0 warnings） |
| `npm run test` | 通过（14 files, 72 tests） |
| `npm run build` | 通过（生产构建成功） |
