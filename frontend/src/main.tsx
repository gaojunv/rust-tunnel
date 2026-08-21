import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App.tsx'
import './index.css'
import './i18n'
import { isIos26Plus } from './lib/ios'

// iOS 26+ 支持 viewport meta 的 interactive-widget=resizes-content：软键盘弹起时
// 布局视口自身收缩，浏览器接管布局与滚动（聚焦输入框自动滚动可见），比
// visualViewport 高度 hack（--vvh，见 useVisualViewportHeight）更稳。追加而非重写，
// 保留 index.html 既有 viewport-fit=cover 等声明。iOS ≤25 与不支持的浏览器忽略该
// 指令，仍走 --vvh 兜底，两者不冲突。
if (isIos26Plus()) {
  const meta = document.querySelector('meta[name="viewport"]');
  if (meta && !meta.getAttribute('content')?.includes('interactive-widget')) {
    meta.setAttribute('content', `${meta.getAttribute('content')}, interactive-widget=resizes-content`);
  }
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
