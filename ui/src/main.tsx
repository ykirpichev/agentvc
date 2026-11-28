import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router'
import { LiveProvider } from './api/live'
import { App } from './App'
import { basePath } from './core/base'
import { HotkeyProvider } from './keyboard/hotkeys'
import { ThemeProvider } from './theme/theme'
import './styles.css'
import './review.css'
import './graph.css'
import './native.css'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ThemeProvider>
      <HotkeyProvider>
        <LiveProvider>
          <BrowserRouter basename={basePath || undefined}>
            <App />
          </BrowserRouter>
        </LiveProvider>
      </HotkeyProvider>
    </ThemeProvider>
  </StrictMode>,
)
