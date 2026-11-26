// Answers recorded from agentvcd over a demo space with two windows (stacked
// branches) and three versions, keyed by path and query. Re-record with
// tests/record-fixture.py when the API shapes change.
export const FIXTURE: Record<string, unknown> = {
  "/api/space": {
    "workdir": "/home/me/demo9",
    "name": "demo9",
    "format_version": 1,
    "daemon_version": "0.0.1"
  },
  "/api/graph": {
    "upstream": "origin/main",
    "trunk": [
      {
        "commit": "2d4a8c9ed364524b7ed9db7fdbd8b059ba2c21db",
        "summary": "trunk: sub",
        "time": 1795682055,
        "on_upstream": true
      },
      {
        "commit": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
        "summary": "calc: add",
        "time": 1795682054,
        "on_upstream": true
      },
      {
        "commit": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
        "summary": "ops: div",
        "time": 1795682055,
        "on_upstream": false
      }
    ],
    "attempts": [
      {
        "attempt": {
          "id": 1,
          "task_id": null,
          "parent_attempt": null,
          "parent_snapshot": null,
          "actor": "human",
          "model": null,
          "prompt": null,
          "status": "exploring",
          "reason": null,
          "view_path": "/home/me/demo9",
          "backend": "workdir",
          "created_at": 1795682055
        },
        "base_commit": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
        "behind": 1,
        "conflicts": 0,
        "head_snapshot": 1,
        "pr": null
      },
      {
        "attempt": {
          "id": 2,
          "task_id": 1,
          "parent_attempt": 1,
          "parent_snapshot": 1,
          "actor": "agent:claude",
          "model": null,
          "prompt": null,
          "status": "passed",
          "reason": null,
          "view_path": "/home/me/demo9/.agentvc/views/a2",
          "backend": "overlay",
          "created_at": 1795682055
        },
        "base_commit": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
        "behind": 1,
        "conflicts": 0,
        "head_snapshot": 2,
        "pr": null
      },
      {
        "attempt": {
          "id": 3,
          "task_id": 1,
          "parent_attempt": 1,
          "parent_snapshot": 1,
          "actor": "agent:codex",
          "model": null,
          "prompt": null,
          "status": "failed",
          "reason": "check exited 1",
          "view_path": "/home/me/demo9/.agentvc/views/a3",
          "backend": "overlay",
          "created_at": 1795682055
        },
        "base_commit": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
        "behind": 1,
        "conflicts": 0,
        "head_snapshot": 4,
        "pr": null
      }
    ]
  },
  "/api/tasks": [
    {
      "id": 1,
      "goal": "Format results nicely",
      "criteria": "ints print without .0",
      "created_at": 1795682055,
      "attempts": {
        "total": 2,
        "exploring": 0,
        "passed": 1,
        "failed": 1,
        "abandoned": 0,
        "promoted": 0
      },
      "updated_at": 1795682055
    }
  ],
  "/api/live": [],
  "/api/workspaces": {
    "here": "w3247c1f6",
    "workspaces": [
      {
        "id": "w3247c1f6",
        "name": "main-window",
        "kind": "main",
        "machine": "vm",
        "path": "/home/me/demo9",
        "here": true,
        "state": "idle",
        "last_seen": 1795682055,
        "current": "main",
        "trunk": "main",
        "base_ref": "origin/main",
        "base": "2d4a8c9ed364524b7ed9db7fdbd8b059ba2c21db",
        "pending": null,
        "branches": [
          {
            "name": "mul",
            "workspace": "w3247c1f6",
            "parent": "main",
            "parent_kind": "trunk",
            "parent_workspace": null,
            "parent_rev": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
            "tip": "e25bea7bd08fd53e0b81265bb34d69470611ccc3",
            "commits": 1,
            "needs_restack": true,
            "behind": 1,
            "conflicts": 0,
            "current": false,
            "depth": 0,
            "children": [
              "div"
            ],
            "pr": null,
            "attempt": null,
            "task": null,
            "status": null
          },
          {
            "name": "div",
            "workspace": "w3247c1f6",
            "parent": "mul",
            "parent_kind": "stack",
            "parent_workspace": "w3247c1f6",
            "parent_rev": "e25bea7bd08fd53e0b81265bb34d69470611ccc3",
            "tip": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
            "commits": 1,
            "needs_restack": false,
            "behind": 1,
            "conflicts": 0,
            "current": false,
            "depth": 1,
            "children": [
              "cli-ops"
            ],
            "pr": null,
            "attempt": null,
            "task": null,
            "status": null
          },
          {
            "name": "cli-ops",
            "workspace": "w3247c1f6",
            "parent": "div",
            "parent_kind": "stack",
            "parent_workspace": "w3247c1f6",
            "parent_rev": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
            "tip": "8c1978599ed5fa2be1b83abca2de8b34d2ad7f8e",
            "commits": 1,
            "needs_restack": false,
            "behind": 1,
            "conflicts": 0,
            "current": false,
            "depth": 2,
            "children": [],
            "pr": null,
            "attempt": null,
            "task": null,
            "status": null
          }
        ],
        "requests": []
      },
      {
        "id": "wf41d4ab4",
        "name": "docs",
        "kind": "worktree",
        "machine": "vm",
        "path": "/home/me/demo9-b",
        "here": false,
        "state": "idle",
        "last_seen": 1795682056,
        "current": "usage-docs",
        "trunk": "main",
        "base_ref": "origin/main",
        "base": "2d4a8c9ed364524b7ed9db7fdbd8b059ba2c21db",
        "pending": null,
        "branches": [
          {
            "name": "usage-docs",
            "workspace": "wf41d4ab4",
            "parent": "main",
            "parent_kind": "trunk",
            "parent_workspace": null,
            "parent_rev": "2d4a8c9ed364524b7ed9db7fdbd8b059ba2c21db",
            "tip": "8d8541e00534eb6049330dbece0cff961bb7f2e4",
            "commits": 1,
            "needs_restack": false,
            "behind": 0,
            "conflicts": 0,
            "current": true,
            "depth": 0,
            "children": [],
            "pr": null,
            "attempt": null,
            "task": null,
            "status": null
          }
        ],
        "requests": []
      }
    ]
  },
  "/api/diff?b=a1": {
    "a": null,
    "b": {
      "id": 1,
      "attempt_id": 1,
      "tree": "13398cd4cdde5a2d3ff26f0493e3558eceef886f",
      "trigger": "init",
      "meta": null,
      "created_at": 1795682055
    },
    "a_ref": "git:c6123de16de21d80888c3e01ea9948a3eedfe1ec",
    "b_ref": "s1",
    "changes": []
  },
  "/api/diff?b=a2": {
    "a": {
      "id": 1,
      "attempt_id": 1,
      "tree": "13398cd4cdde5a2d3ff26f0493e3558eceef886f",
      "trigger": "init",
      "meta": null,
      "created_at": 1795682055
    },
    "b": {
      "id": 2,
      "attempt_id": 2,
      "tree": "1c173f5d38fd1fd1a72b1282b68ad0fdffd05e65",
      "trigger": "run-exit",
      "meta": {
        "check_exit_code": 0,
        "command": [
          "sh",
          "-c",
          "printf \"def fmt(x):\\n    return str(int(x)) if x == int(x) else str(x)\\n\" > calc/fmt.py"
        ],
        "duration_ms": 3,
        "exit_code": 0,
        "run": 1
      },
      "created_at": 1795682055
    },
    "a_ref": "s1",
    "b_ref": "s2",
    "changes": [
      {
        "path": "calc/fmt.py",
        "kind": "added"
      }
    ]
  },
  "/api/file?ref=s2&path=calc%2Ffmt.py": {
    "ref": "s2",
    "snapshot": 2,
    "path": "calc/fmt.py",
    "mode": "file",
    "oid": "db4a992597365bdceefe03d4fb7c8da5e2ef95d8",
    "size": 62,
    "binary": false,
    "too_large": false,
    "content": "def fmt(x):\n    return str(int(x)) if x == int(x) else str(x)\n"
  },
  "/api/diff?b=a3": {
    "a": null,
    "b": {
      "id": 4,
      "attempt_id": 3,
      "tree": "932b22da783ed14f9890526aa1a385f1bf5dff63",
      "trigger": "rebase",
      "meta": {
        "from": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
        "onto": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
        "onto_ref": "div"
      },
      "created_at": 1795682056
    },
    "a_ref": "git:652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
    "b_ref": "s4",
    "changes": [
      {
        "path": "calc/fmt.py",
        "kind": "added"
      }
    ]
  },
  "/api/file?ref=s4&path=calc%2Ffmt.py": {
    "ref": "s4",
    "snapshot": 4,
    "path": "calc/fmt.py",
    "mode": "file",
    "oid": "1c83ea063b062fbfdd906efed49207f7574d4c47",
    "size": 27,
    "binary": false,
    "too_large": false,
    "content": "def fmt(x): return repr(x)\n"
  },
  "/api/workspaces/w3247c1f6/branches/mul": {
    "workspace": "w3247c1f6",
    "name": "mul",
    "parent": "main",
    "base": "c6123de16de21d80888c3e01ea9948a3eedfe1ec",
    "tip": "e25bea7bd08fd53e0b81265bb34d69470611ccc3",
    "current": false,
    "commits": [
      {
        "commit": "e25bea7bd08fd53e0b81265bb34d69470611ccc3",
        "subject": "ops: mul",
        "author": "demo",
        "time": 1795682055
      }
    ],
    "files": [
      {
        "path": "calc/ops.py",
        "kind": "modified"
      }
    ]
  },
  "/api/file?ref=git%3Ac6123de16de21d80888c3e01ea9948a3eedfe1ec&path=calc%2Fops.py": {
    "ref": "git:c6123de16de21d80888c3e01ea9948a3eedfe1ec",
    "snapshot": null,
    "path": "calc/ops.py",
    "mode": "file",
    "oid": "4693ad3cf8b0903b98497fb89b8b524fbf1b93f4",
    "size": 32,
    "binary": false,
    "too_large": false,
    "content": "def add(a, b):\n    return a + b\n"
  },
  "/api/file?ref=git%3Ae25bea7bd08fd53e0b81265bb34d69470611ccc3&path=calc%2Fops.py": {
    "ref": "git:e25bea7bd08fd53e0b81265bb34d69470611ccc3",
    "snapshot": null,
    "path": "calc/ops.py",
    "mode": "file",
    "oid": "717f7eda038113a5fc8bc24c9035edd1e2aa3e62",
    "size": 66,
    "binary": false,
    "too_large": false,
    "content": "def add(a, b):\n    return a + b\n\n\ndef mul(a, b):\n    return a * b\n"
  },
  "/api/workspaces/w3247c1f6/branches/div": {
    "workspace": "w3247c1f6",
    "name": "div",
    "parent": "mul",
    "base": "e25bea7bd08fd53e0b81265bb34d69470611ccc3",
    "tip": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
    "current": false,
    "commits": [
      {
        "commit": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
        "subject": "ops: div",
        "author": "demo",
        "time": 1795682055
      }
    ],
    "files": [
      {
        "path": "calc/ops.py",
        "kind": "modified"
      }
    ]
  },
  "/api/file?ref=git%3A652c29a708b8827cd7c730a4acf5ba0859cf2e9e&path=calc%2Fops.py": {
    "ref": "git:652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
    "snapshot": null,
    "path": "calc/ops.py",
    "mode": "file",
    "oid": "e8ab70870ceb0f08617692775ad800f84a43dd74",
    "size": 100,
    "binary": false,
    "too_large": false,
    "content": "def add(a, b):\n    return a + b\n\n\ndef mul(a, b):\n    return a * b\n\n\ndef div(a, b):\n    return a / b\n"
  },
  "/api/workspaces/w3247c1f6/branches/cli-ops": {
    "workspace": "w3247c1f6",
    "name": "cli-ops",
    "parent": "div",
    "base": "652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
    "tip": "8c1978599ed5fa2be1b83abca2de8b34d2ad7f8e",
    "current": false,
    "commits": [
      {
        "commit": "8c1978599ed5fa2be1b83abca2de8b34d2ad7f8e",
        "subject": "cli: expose mul and div",
        "author": "demo",
        "time": 1795682055
      }
    ],
    "files": [
      {
        "path": "calc/cli.py",
        "kind": "modified"
      }
    ]
  },
  "/api/file?ref=git%3A652c29a708b8827cd7c730a4acf5ba0859cf2e9e&path=calc%2Fcli.py": {
    "ref": "git:652c29a708b8827cd7c730a4acf5ba0859cf2e9e",
    "snapshot": null,
    "path": "calc/cli.py",
    "mode": "file",
    "oid": "91e58dd7b1c6873adf4035685c1b424dd3804fbf",
    "size": 42,
    "binary": false,
    "too_large": false,
    "content": "OPS = [\"add\"]\n\ndef main():\n    print(OPS)\n"
  },
  "/api/file?ref=git%3A8c1978599ed5fa2be1b83abca2de8b34d2ad7f8e&path=calc%2Fcli.py": {
    "ref": "git:8c1978599ed5fa2be1b83abca2de8b34d2ad7f8e",
    "snapshot": null,
    "path": "calc/cli.py",
    "mode": "file",
    "oid": "9e84bd239c7b778648a63f233e831eee90f8e139",
    "size": 56,
    "binary": false,
    "too_large": false,
    "content": "OPS = [\"add\", \"mul\", \"div\"]\n\ndef main():\n    print(OPS)\n"
  },
  "/api/workspaces/wf41d4ab4/branches/usage-docs": {
    "workspace": "wf41d4ab4",
    "name": "usage-docs",
    "parent": "main",
    "base": "2d4a8c9ed364524b7ed9db7fdbd8b059ba2c21db",
    "tip": "8d8541e00534eb6049330dbece0cff961bb7f2e4",
    "current": true,
    "commits": [
      {
        "commit": "8d8541e00534eb6049330dbece0cff961bb7f2e4",
        "subject": "docs: usage",
        "author": "demo",
        "time": 1795682056
      }
    ],
    "files": [
      {
        "path": "USAGE.md",
        "kind": "added"
      }
    ]
  },
  "/api/file?ref=git%3A8d8541e00534eb6049330dbece0cff961bb7f2e4&path=USAGE.md": {
    "ref": "git:8d8541e00534eb6049330dbece0cff961bb7f2e4",
    "snapshot": null,
    "path": "USAGE.md",
    "mode": "file",
    "oid": "618241fc8decc2faeafa7c1ae18d8ace29017739",
    "size": 8,
    "binary": false,
    "too_large": false,
    "content": "# usage\n"
  }
}
