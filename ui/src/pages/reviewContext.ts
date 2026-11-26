import { useOutletContext } from 'react-router'
import type { Graph, TaskSummary } from '../api/types'

export interface ReviewContext {
  graph: Graph | undefined
  tasks: TaskSummary[] | undefined
}

export const useReview = () => useOutletContext<ReviewContext>()

