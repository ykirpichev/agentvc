import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The daemon (`agentvcd`) serves the API on 127.0.0.1:7420 by default.
// Override with AGENTVCD_URL=http://localhost:PORT npm run dev.
const daemon = process.env.AGENTVCD_URL ?? 'http://127.0.0.1:7420'

export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    proxy: {
      '/api': { target: daemon, ws: true },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
