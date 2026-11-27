import { useOutletContext } from 'react-router'
import type { AgentList, Graph, TaskSummary } from '../api/types'

export interface ReviewContext {
  graph: Graph | undefined
  tasks: TaskSummary[] | undefined
  /** Agents on the workspaces' stacks; undefined while loading. */
  agents?: AgentList
}

export const useReview = () => useOutletContext<ReviewContext>()

