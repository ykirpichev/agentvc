import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router'
import { LiveProvider } from './api/live'
import { App } from './App'
import { HotkeyProvider } from './keyboard/hotkeys'
import { ThemeProvider } from './theme/theme'
import './styles.css'
import './review.css'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ThemeProvider>
      <HotkeyProvider>
        <LiveProvider>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </LiveProvider>
      </HotkeyProvider>
    </ThemeProvider>
  </StrictMode>,
)
