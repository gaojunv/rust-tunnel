import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";

// Tauri 期望的前端配置：固定端口 1420（与 src-tauri/tauri.conf.json 约定），
// build.target = esnext（系统 webview 已足够现代，无需兼容老浏览器）。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_"],
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "esnext",
  },
  resolve: {
    alias: {
      "@": path.resolve(process.cwd(), "./src"),
    },
  },
});
