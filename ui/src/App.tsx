import { BranchReview } from './pages/BranchReview'
import { Navigate, Route, Routes, useParams } from 'react-router'
import { Empty } from './components/bits'
import { Shell } from './components/Shell'
import { AttemptPage } from './pages/AttemptPage'
import { Compare } from './pages/Compare'
import { Find } from './pages/Find'
import { CommitReview, ReviewHome, TaskReview, VersionReview } from './pages/Review'
import { AgentPage } from './pages/Agents'

export function App() {
  return (
    <Routes>
      <Route element={<Shell />}>
        <Route index element={<Navigate to="/review" replace />} />
        <Route path="review" element={<ReviewHome />} />
        <Route path="review/t/:id" element={<TaskReview />} />
        <Route path="review/v/:id" element={<VersionReview />} />
        <Route path="review/c/:sha" element={<CommitReview />} />
        <Route path="review/b/:workspace/*" element={<BranchReview />} />
        <Route path="review/a/:id" element={<AgentPage />} />
        {/* Old addresses: the tree and the review pages cover them now. */}
        <Route path="inbox" element={<Navigate to="/review" replace />} />
        <Route path="tasks" element={<Navigate to="/review" replace />} />
        <Route path="tasks/:id" element={<ToTaskReview />} />
        <Route path="attempts/:id" element={<AttemptPage />} />
        <Route path="compare" element={<Compare />} />
        <Route path="live" element={<Navigate to="/review" replace />} />
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

function ToTaskReview() {
  const { id = '' } = useParams()
  return <Navigate to={`/review/t/${id}`} replace />
}
