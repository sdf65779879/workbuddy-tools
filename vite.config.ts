import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 期望固定端口，开发服务器不稳定时自动跳过端口检查
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  // 阻止 vite 混淆 asset URL，Tauri 对前端资源有自己的一套处理
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 不监听 Rust 侧变化
      ignored: ["**/src-tauri/**"],
    },
  },
});
