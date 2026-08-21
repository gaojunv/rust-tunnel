import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App.tsx'
import './index.css'
import './i18n'

// --vh 视口高度变量（对标 Kimi 的 vhUnitFix）：用 window.innerHeight 驱动而非
// 依赖 visualViewport。iOS PWA 键盘弹出/收起时 innerHeight 跟随可视视口收缩/恢复，
// 且不踩 WebKit bug 254861（visualViewport 在 installed PWA 里 dismiss 后停滞）。
// 用法：CSS 里 height: calc(var(--vh, 1vh) * 100)，--vh 缺省回退 1vh。
function setVhUnit() {
  document.documentElement.style.setProperty('--vh', `${window.innerHeight * 0.01}px`)
}
setVhUnit()
window.addEventListener('resize', setVhUnit)
window.addEventListener('orientationchange', setVhUnit)

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
