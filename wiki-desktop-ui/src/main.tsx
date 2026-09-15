import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { applyTheme, getTheme } from "./lib/theme";
import { isTauri } from "./api/tauri";

// 非 Tauri 环境启用内存假服务器（拦截 mock://local 的 fetch）。
// mock 模块动态导入：Tauri 生产包 tree-shaking 后不含 mock chunk 的引用，
// 打包器将其拆为独立 chunk，Tauri 内永不加载。
if (!isTauri) {
  void import("./api/mock").then((m) => m.installMockServerInterceptor());
}

applyTheme(getTheme());

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
