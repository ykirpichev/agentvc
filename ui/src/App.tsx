import { Navigate, Route, Routes } from 'react-router'
import { Empty } from './components/bits'
import { Shell } from './components/Shell'
import { AttemptPage } from './pages/AttemptPage'
import { Compare } from './pages/Compare'
import { Find } from './pages/Find'
import { Inbox } from './pages/Inbox'
import { Live } from './pages/Live'
import { TaskPage } from './pages/TaskPage'
import { Tasks } from './pages/Tasks'

export function App() {
  return (
    <Routes>
      <Route element={<Shell />}>
        <Route index element={<Navigate to="/inbox" replace />} />
        <Route path="inbox" element={<Inbox />} />
        <Route path="tasks" element={<Tasks />} />
        <Route path="tasks/:id" element={<TaskPage />} />
        <Route path="attempts/:id" element={<AttemptPage />} />
        <Route path="compare" element={<Compare />} />
        <Route path="live" element={<Live />} />
        <Route path="find" element={<Find />} />
        <Route
          path="*"
          element={
            <div className="page">
              <Empty title="Not found">That page does not exist.</Empty>
            </div>
          }
        />
      </Route>
    </Routes>
  )
}
