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
    rollupOptions: {
      output: {
        // 首屏瘦身：重型依赖按组拆分，首屏只留 react/vendor 主包。
        // - vendor-react：react 生态（首屏必需，常驻）
        // - vendor-editor：CodeMirror 全套（编辑器用，首屏必需但独立成块）
        // - vendor-markdown：katex/streamdown/diff + react-markdown 系
        //   （仅 AgentPanel/SelectionToolbar/公式 widget 消费，经 React.lazy/
        //   动态 import 后不再进首屏）
        // - vendor-mock：mock.ts（仅非 Tauri 动态加载，生产主包无引用）
        manualChunks(id) {
          // mock.ts 源码文件（非 node_modules）：单独立 chunk，非 Tauri 动态加载
          if (id.includes("src/api/mock")) {
            return "vendor-mock";
          }
          if (id.includes("node_modules")) {
            if (
              id.includes("/react/") ||
              id.includes("/react-dom/") ||
              id.includes("/scheduler/")
            ) {
              return "vendor-react";
            }
            if (
              id.includes("/katex") ||
              id.includes("/streamdown") ||
              id.includes("/react-markdown") ||
              id.includes("/remark-") ||
              id.includes("/rehype-") ||
              id.includes("/unified") ||
              id.includes("/hast") ||
              id.includes("/mdast") ||
              id.includes("/micromark") ||
              id.includes("/diff") ||
              id.includes("/parse-diff") ||
              id.includes("/diff2html")
            ) {
              return "vendor-markdown";
            }
            if (id.includes("/@codemirror/") || id.includes("/@lezer/") || id.includes("/codemirror")) {
              return "vendor-editor";
            }
          }
          return undefined;
        },
      },
    },
  },
  resolve: {
    alias: {
      "@": path.resolve(process.cwd(), "./src"),
    },
  },
});
