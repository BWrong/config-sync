import { defineConfig } from "vite";

// Tauri 的 devUrl 固定指向 5173，端口被占时直接失败而不是悄悄换一个。
export default defineConfig({
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
});
