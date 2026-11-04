import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// This is server-side configuration, never a VITE_* variable in the bundle.
// Vite runs from web/, so explicitly pass the backend port from your shell.
export default defineConfig({
  plugins: [vue()],
  server: {
    port: 3000,
    strictPort: true,
    proxy: {
      '/api': {
        target: process.env.API_PROXY_TARGET ?? `http://127.0.0.1:${process.env.BIND_PORT ?? '8080'}`,
        rewrite: (path) => path.replace(/^\/api(?=\/|$)/, ''),
      },
    },
  },
})
