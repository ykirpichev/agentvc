// Answers recorded from agentvcd over a small demo space (view paths dropped).
export const FIXTURE: Record<string, unknown> = {
  "/api/space": {
    "workdir": "/home/me/demo",
    "name": "demo",
    "format_version": 1,
    "daemon_version": "0.0.1"
  },
  "/api/tasks": [
    {
      "id": 1,
      "goal": "Fix flaky login test",
      "criteria": null,
      "created_at": 1795645270,
      "attempts": {
        "total": 4,
        "exploring": 0,
        "passed": 0,
        "failed": 0,
        "abandoned": 3,
        "promoted": 1
      },
      "updated_at": 1795645276
    },
    {
      "id": 2,
      "goal": "Add CSV export to reports",
      "criteria": null,
      "created_at": 1795645270,
      "attempts": {
        "total": 3,
        "exploring": 3,
        "passed": 0,
        "failed": 0,
        "abandoned": 0,
        "promoted": 0
      },
      "updated_at": 1795645277
    }
  ],
  "/api/attempts": [
    {
      "id": 1,
      "task_id": null,
      "parent_attempt": null,
      "parent_snapshot": null,
      "actor": "human",
      "model": null,
      "prompt": null,
      "status": "exploring",
      "reason": null,
      "backend": "workdir",
      "created_at": 1795645264
    },
    {
      "id": 2,
      "task_id": 1,
      "parent_attempt": 1,
      "parent_snapshot": 1,
      "actor": "agent:claude-code",
      "model": "claude-opus",
      "prompt": "fix flake",
      "status": "abandoned",
      "reason": "lost to a3",
      "backend": "overlay",
      "created_at": 1795645270
    },
    {
      "id": 3,
      "task_id": 1,
      "parent_attempt": 1,
      "parent_snapshot": 1,
      "actor": "agent:claude-code",
      "model": "codex",
      "prompt": "fix flake",
      "status": "promoted",
      "reason": null,
      "backend": "overlay",
      "created_at": 1795645270
    },
    {
      "id": 4,
      "task_id": 1,
      "parent_attempt": 1,
      "parent_snapshot": 1,
      "actor": "agent:claude-code",
      "model": "gemini",
      "prompt": "fix flake",
      "status": "abandoned",
      "reason": "rewrote auth module, too broad",
      "backend": "overlay",
      "created_at": 1795645270
    },
    {
      "id": 5,
      "task_id": 2,
      "parent_attempt": 1,
      "parent_snapshot": 1,
      "actor": "agent:claude-code",
      "model": "claude-opus",
      "prompt": null,
      "status": "exploring",
      "reason": null,
      "backend": "overlay",
      "created_at": 1795645270
    },
    {
      "id": 6,
      "task_id": 2,
      "parent_attempt": 1,
      "parent_snapshot": 1,
      "actor": "agent:claude-code",
      "model": "claude-sonnet",
      "prompt": null,
      "status": "exploring",
      "reason": null,
      "backend": "overlay",
      "created_at": 1795645270
    },
    {
      "id": 7,
      "task_id": 1,
      "parent_attempt": 2,
      "parent_snapshot": 2,
      "actor": "agent:claude-code",
      "model": "claude-opus",
      "prompt": "refine: also fix timeout",
      "status": "abandoned",
      "reason": "lost to a3",
      "backend": "overlay",
      "created_at": 1795645276
    },
    {
      "id": 8,
      "task_id": 2,
      "parent_attempt": 5,
      "parent_snapshot": 5,
      "actor": "agent",
      "model": "claude-opus",
      "prompt": null,
      "status": "exploring",
      "reason": null,
      "backend": "overlay",
      "created_at": 1795645277
    }
  ],
  "/api/live": [
    {
      "attempt": {
        "id": 8,
        "task_id": 2,
        "parent_attempt": 5,
        "parent_snapshot": 5,
        "actor": "agent",
        "model": "claude-opus",
        "prompt": null,
        "status": "exploring",
        "reason": null,
        "backend": "overlay",
        "created_at": 1795645277
      },
      "task_goal": "Add CSV export to reports",
      "head": null,
      "stats": {
        "attempt": 8,
        "snapshots": 0,
        "check": null,
        "last_exit_code": null,
        "cost_usd": null,
        "tokens": null,
        "duration_s": 0,
        "run_ms": null,
        "summary": null,
        "last_activity": 1795645277
      },
      "files_changed": 0,
      "files": [],
      "overlaps": {},
      "run": {
        "id": 1,
        "command": [
          "bash",
          "-c",
          "for i in $(seq 1 600); do echo step $i; sleep 1; done"
        ],
        "check_command": null,
        "started_at": 1795645277,
        "alive": true,
        "log_tail": []
      }
    },
    {
      "attempt": {
        "id": 6,
        "task_id": 2,
        "parent_attempt": 1,
        "parent_snapshot": 1,
        "actor": "agent:claude-code",
        "model": "claude-sonnet",
        "prompt": null,
        "status": "exploring",
        "reason": null,
        "backend": "overlay",
        "created_at": 1795645270
      },
      "task_goal": "Add CSV export to reports",
      "head": {
        "id": 6,
        "attempt_id": 6,
        "tree": "27fe8fa9c66a18a016d055430f487f28c49a4db1",
        "trigger": "manual",
        "meta": {
          "cost_usd": "0.46",
          "summary": "retry with backoff in a6"
        },
        "created_at": 1795645276
      },
      "stats": {
        "attempt": 6,
        "snapshots": 1,
        "check": null,
        "last_exit_code": null,
        "cost_usd": 0.46,
        "tokens": null,
        "duration_s": 6,
        "run_ms": null,
        "summary": "retry with backoff in a6",
        "last_activity": 1795645276
      },
      "files_changed": 2,
      "files": [
        {
          "path": "login_test.rs",
          "kind": "added"
        },
        {
          "path": "main.rs",
          "kind": "modified"
        }
      ],
      "overlaps": {
        "login_test.rs": [
          5
        ],
        "main.rs": [
          5
        ]
      },
      "run": null
    },
    {
      "attempt": {
        "id": 5,
        "task_id": 2,
        "parent_attempt": 1,
        "parent_snapshot": 1,
        "actor": "agent:claude-code",
        "model": "claude-opus",
        "prompt": null,
        "status": "exploring",
        "reason": null,
        "backend": "overlay",
        "created_at": 1795645270
      },
      "task_goal": "Add CSV export to reports",
      "head": {
        "id": 5,
        "attempt_id": 5,
        "tree": "8f2b6dffc51fec1f4afba5958a7ba05b1dae337f",
        "trigger": "manual",
        "meta": {
          "cost_usd": "0.45",
          "summary": "retry with backoff in a5"
        },
        "created_at": 1795645276
      },
      "stats": {
        "attempt": 5,
        "snapshots": 1,
        "check": null,
        "last_exit_code": null,
        "cost_usd": 0.45,
        "tokens": null,
        "duration_s": 6,
        "run_ms": null,
        "summary": "retry with backoff in a5",
        "last_activity": 1795645276
      },
      "files_changed": 2,
      "files": [
        {
          "path": "login_test.rs",
          "kind": "added"
        },
        {
          "path": "main.rs",
          "kind": "modified"
        }
      ],
      "overlaps": {
        "login_test.rs": [
          6
        ],
        "main.rs": [
          6
        ]
      },
      "run": null
    }
  ],
  "/api/inbox": [],
  "/api/tasks/1/stats": [
    {
      "attempt": 2,
      "snapshots": 1,
      "check": null,
      "last_exit_code": null,
      "cost_usd": 0.42,
      "tokens": null,
      "duration_s": 6,
      "run_ms": null,
      "summary": "retry with backoff in a2",
      "last_activity": 1795645276
    },
    {
      "attempt": 3,
      "snapshots": 1,
      "check": null,
      "last_exit_code": null,
      "cost_usd": 0.43,
      "tokens": null,
      "duration_s": 6,
      "run_ms": null,
      "summary": "retry with backoff in a3",
      "last_activity": 1795645276
    },
    {
      "attempt": 4,
      "snapshots": 1,
      "check": null,
      "last_exit_code": null,
      "cost_usd": 0.44,
      "tokens": null,
      "duration_s": 6,
      "run_ms": null,
      "summary": "retry with backoff in a4",
      "last_activity": 1795645276
    },
    {
      "attempt": 7,
      "snapshots": 0,
      "check": null,
      "last_exit_code": null,
      "cost_usd": null,
      "tokens": null,
      "duration_s": 0,
      "run_ms": null,
      "summary": null,
      "last_activity": 1795645276
    }
  ],
  "/api/tasks/2/stats": [
    {
      "attempt": 5,
      "snapshots": 1,
      "check": null,
      "last_exit_code": null,
      "cost_usd": 0.45,
      "tokens": null,
      "duration_s": 6,
      "run_ms": null,
      "summary": "retry with backoff in a5",
      "last_activity": 1795645276
    },
    {
      "attempt": 6,
      "snapshots": 1,
      "check": null,
      "last_exit_code": null,
      "cost_usd": 0.46,
      "tokens": null,
      "duration_s": 6,
      "run_ms": null,
      "summary": "retry with backoff in a6",
      "last_activity": 1795645276
    },
    {
      "attempt": 8,
      "snapshots": 0,
      "check": null,
      "last_exit_code": null,
      "cost_usd": null,
      "tokens": null,
      "duration_s": 0,
      "run_ms": null,
      "summary": null,
      "last_activity": 1795645277
    }
  ]
}
