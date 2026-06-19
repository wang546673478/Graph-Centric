import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const backendPort = process.env.BACKEND_PORT || '8080'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      '/api': `http://localhost:${backendPort}`,
      '/ws': { target: `ws://localhost:${backendPort}`, ws: true },
    },
  },
  build: {
    outDir: 'dist',
  },
})
