import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The daemon (`agentvcd`) serves the API on 127.0.0.1:7420 by default.
// Override with AGENTVCD_URL=http://localhost:PORT npm run dev.
const daemon = process.env.AGENTVCD_URL ?? 'http://127.0.0.1:7420'

// Changes need the daemon's token (#199). The dev server adds it from
// `$AGENTVC_HOME/agentvcd.token` to calls made by its own pages only.
const tokenFile = join(process.env.AGENTVC_HOME || join(homedir(), '.agentvc'), 'agentvcd.token')
function daemonToken(): string | null {
  try {
    return readFileSync(tokenFile, 'utf8').trim() || null
  } catch {
    return null
  }
}

export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    proxy: {
      '/api': {
        target: daemon,
        ws: true,
        configure: (proxy) => {
          proxy.on('proxyReq', (proxyReq, req) => {
            const { origin, host } = req.headers
            const token = origin && host && origin === `http://${host}` ? daemonToken() : null
            if (token) proxyReq.setHeader('x-agentvc-token', token)
          })
        },
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
