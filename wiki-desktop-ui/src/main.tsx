import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { applyTheme, getTheme } from "./lib/theme";
import { isTauri } from "./api/tauri";
import { installMockServerInterceptor } from "./api/mock";

// 非 Tauri 环境启用内存假服务器（拦截 mock://local 的 fetch）
if (!isTauri) {
  installMockServerInterceptor();
}

applyTheme(getTheme());

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
