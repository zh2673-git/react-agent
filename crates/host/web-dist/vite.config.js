import { defineConfig } from 'vite';

// 前端 dev server：serve 当前目录的 index.html，并把 /api 反代到后端（cargo run 的 8710）。
// 用法：cd crates/host/web-dist && npm install && npm run dev
export default defineConfig({
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8710',
        changeOrigin: true,
      },
    },
  },
});
