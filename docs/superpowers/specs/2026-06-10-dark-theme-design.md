# 前端跟随系统的黑夜主题设计

## 背景

rust-tunnel 前端当前使用 React、TypeScript、Vite 和 Tailwind CSS。页面样式主要写在组件的 Tailwind class 中，尚未实现主题系统，也没有监听 `prefers-color-scheme`。本设计为前端管理界面增加完整黑夜主题，并支持默认跟随系统、用户手动覆盖和刷新后保持偏好。

## 目标

- 默认跟随操作系统浅色/深色主题变化。
- 提供页面内三态切换：跟随系统、浅色、深色。
- 用户选择保存到浏览器 `localStorage`，刷新后保持。
- 深色风格采用中性深灰 / 蓝灰，适合管理后台长时间使用。
- 覆盖整个前端管理界面，包括登录页、导航栏、仪表盘、客户端列表、流量/质量图表、Shadowsocks/Trojan 配置和日志页面。
- 不改变后端 API、数据库结构或认证流程。

## 非目标

- 不做多套品牌主题或自定义配色编辑器。
- 不把主题偏好同步到服务器。
- 不引入新的 UI 组件库或全局状态管理库。
- 不重构前端路由或页面组织方式。

## 用户体验

主题入口放在导航栏右侧，使用图标按钮和下拉菜单：

- `system`：显示系统/电脑图标，表示跟随系统。
- `light`：显示太阳图标。
- `dark`：显示月亮图标。

点击按钮后显示三个选项：

1. 跟随系统
2. 浅色
3. 深色

当前选项在菜单中高亮或显示勾选标记。用户选择后页面立即切换主题并保存偏好。

首次访问时默认使用 `system`。在 `system` 模式下，如果操作系统主题从浅色切换到深色，页面应实时切到深色；反向切换也应实时生效。用户选择 `light` 或 `dark` 后，页面固定为对应主题，不再随系统变化，直到用户重新选择 `system`。

## 架构

采用 Tailwind 的 class 模式实现深色主题：

- 在 `frontend/tailwind.config.js` 中启用 `darkMode: 'class'`。
- 在应用根部通过 React 主题模块管理用户偏好和生效主题。
- 生效主题为深色时，给 `document.documentElement` 添加 `dark` class。
- 生效主题为浅色时，移除 `document.documentElement` 上的 `dark` class。
- 组件通过 Tailwind `dark:` 变体响应根节点状态。

建议新增主题相关代码：

- `ThemeProvider`：负责初始化主题、监听系统主题变化、读写 `localStorage`、维护主题状态。
- `useTheme`：供组件读取当前偏好和设置偏好。
- `ThemeToggle`：导航栏中的主题切换按钮和菜单。

主题偏好类型固定为：

```ts
type ThemePreference = 'system' | 'light' | 'dark';
type ResolvedTheme = 'light' | 'dark';
```

`ThemeProvider` 对外暴露：

- `preference: ThemePreference`
- `resolvedTheme: ResolvedTheme`
- `setPreference(preference: ThemePreference): void`

## 数据流

1. 应用启动时读取 `localStorage['rust-tunnel-theme']`。
2. 如果值是 `light` 或 `dark`，将其作为用户偏好并直接应用对应主题。
3. 如果值不存在、为 `system` 或非法值，使用 `system`。
4. `system` 模式下读取 `window.matchMedia('(prefers-color-scheme: dark)')` 得到当前生效主题。
5. `system` 模式下监听系统主题变化，并实时更新 `<html>` 的 `dark` class。
6. 用户通过 `ThemeToggle` 选择主题偏好后：
   - 更新 React 状态；
   - 写入 `localStorage['rust-tunnel-theme']`；
   - 立即应用新的 `<html>` class；
   - 如选择 `system`，重新按系统主题解析生效主题。

## 视觉规范

浅色主题保持当前视觉风格为主，避免无关改动。

深色主题采用中性深灰 / 蓝灰：

- 页面背景：`dark:bg-slate-900` 或接近色。
- 卡片背景：`dark:bg-slate-800`。
- 次级区域背景：`dark:bg-slate-800/50`、`dark:bg-slate-900`。
- 边框：`dark:border-slate-700`。
- 主文本：`dark:text-slate-100`。
- 次级文本：`dark:text-slate-300` 或 `dark:text-slate-400`。
- Hover 背景：`dark:hover:bg-slate-700` 或 `dark:hover:bg-slate-800`。
- 输入框：深色背景、清晰边框、浅色文字和 placeholder。
- 状态色保持语义：成功偏绿、警告偏黄、错误偏红、信息偏蓝；在深色背景下调整文字和背景透明度以保证可读性。

图表和统计组件应重点检查：

- 图表容器背景和边框。
- 图例和轴标签文字颜色。
- Tooltip 背景、边框和文字颜色。
- 流量、质量、在线状态等状态色在深色背景下的对比度。

## 组件改造范围

需要覆盖整个前端管理界面。实现时应系统扫描 `frontend/src` 下的 Tailwind 颜色类，重点处理：

- 页面根容器：背景和默认文字颜色。
- 登录页：表单、输入框、按钮、错误提示。
- 导航栏：背景、边框、文字、移动端菜单、主题切换入口。
- 共享组件：`StatCard`、`ChartContainer`、`TimeRangeSelector`、`MobileBottomNav`。
- 仪表盘和客户端列表：卡片、表格、标签、状态徽章、空状态。
- 流量与质量页面：图表容器、筛选控件、指标卡片。
- Shadowsocks/Trojan 配置页面：表单、开关、说明文字、保存状态。
- 日志页面：过滤器、日志列表、级别标签、分页或加载状态。

## 异常处理

- `localStorage` 读取失败：使用 `system`，不中断页面渲染。
- `localStorage` 写入失败：仍在当前会话中切换主题，但不保证刷新后保持。
- `localStorage` 中存在非法值：忽略该值并按 `system` 处理。
- `matchMedia` 不存在：`system` 模式退回浅色；手动 `light` 和 `dark` 仍可工作。
- 主题切换不触发后端请求，后端不可用时仍可切换主题。

## 测试与验证

自动检查：

- 在 `frontend/` 下运行 `npm run build`，确保 TypeScript 和 Vite 构建通过。
- 在 `frontend/` 下运行 `npm run lint`，确保 ESLint 通过。

手动验证：

- 首次打开页面默认跟随系统主题。
- 系统主题从浅色切到深色时，`system` 模式下页面实时变为深色。
- 系统主题从深色切到浅色时，`system` 模式下页面实时变为浅色。
- 选择“深色”后，系统切回浅色时页面仍保持深色。
- 选择“浅色”后，系统切到深色时页面仍保持浅色。
- 选择“跟随系统”后，页面重新按系统主题变化。
- 刷新页面后保留用户选择。
- 登录页、导航栏、仪表盘、客户端列表、图表、SS/Trojan 配置、日志页面在深色模式下文字可读、边框清晰、按钮和 hover 状态正常。

## 实施约束

- 保持当前 React Query v3、Axios 和组件本地状态的架构，不引入全局状态管理库。
- 优先沿用 Tailwind class 写法，避免大规模迁移到 CSS 变量或 CSS Modules。
- 只做与主题相关的局部整理，不做无关重构。
- 主题相关工具函数应保持小而清晰，避免把大量 UI 逻辑放进 `App.tsx`。
