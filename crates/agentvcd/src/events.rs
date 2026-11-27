//! Live events: a poller watches the database for new tasks, attempts,
//! status changes and snapshots, and fans them out to WebSocket clients.
//!
//! Polling is enough for v1: agents and the CLI write the SQLite database
//! from other processes, and a 500 ms poll of three small queries is cheap.

use std::collections::HashMap;
use std::time::Duration;

use agentvc_core::{Attempt, Snapshot, Space, Status, Task};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;

/// One change in the space, as sent over `/api/events`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First message on every connection.
    Hello {
        workdir: String,
    },
    TaskCreated {
        task: Task,
    },
    AttemptCreated {
        attempt: Attempt,
    },
    /// Status or reason changed.
    AttemptUpdated {
        attempt: Attempt,
        previous_status: Status,
    },
    SnapshotCreated {
        snapshot: Snapshot,
    },
    /// A workspace's stack, checked-out branch, branch tips or activity
    /// changed; refetch `/api/workspaces`.
    StackChanged {
        workspace: String,
    },
    /// A workspace (worktree) was added or removed.
    WorkspacesChanged,
    /// A stack edit is waiting for its workspace's agent to stop.
    RequestQueued {
        request_id: String,
        workspace: String,
        reason: Option<String>,
    },
    RequestApplied {
        request_id: String,
        workspace: String,
    },
    RequestFailed {
        request_id: String,
        workspace: String,
        reason: Option<String>,
    },
    RequestCancelled {
        request_id: String,
        workspace: String,
    },
    /// An agent's record changed (state, step, question, result);
    /// refetch `/api/agents`.
    AgentChanged {
        agent: String,
        state: agentvc_core::agent::AgentState,
    },
    /// The client fell behind and missed events; it should refetch.
    Resync,
}

/// What the poller has already seen.
pub(crate) struct Watcher {
    last_task: i64,
    last_snapshot: i64,
    attempts: HashMap<i64, (Status, Option<String>)>,
}

impl Watcher {
    pub(crate) fn new(s: &Space) -> agentvc_core::Result<Watcher> {
        let mut w = Watcher {
            last_task: 0,
            last_snapshot: 0,
            attempts: HashMap::new(),
        };
        w.poll(s)?;
        Ok(w)
    }

    /// Everything that changed since the last call, oldest first.
    pub(crate) fn poll(&mut self, s: &Space) -> agentvc_core::Result<Vec<Event>> {
        let mut out = Vec::new();
        for task in s.tasks()? {
            if task.id > self.last_task {
                self.last_task = task.id;
                out.push(Event::TaskCreated { task });
            }
        }
        for attempt in s.attempts(None)? {
            let now = (attempt.status, attempt.reason.clone());
            match self.attempts.insert(attempt.id, now.clone()) {
                None => out.push(Event::AttemptCreated { attempt }),
                Some(prev) if prev != now => out.push(Event::AttemptUpdated {
                    attempt,
                    previous_status: prev.0,
                }),
                _ => {}
            }
        }
        for snapshot in s.snapshots_after(self.last_snapshot)? {
            self.last_snapshot = snapshot.id;
            out.push(Event::SnapshotCreated { snapshot });
        }
        Ok(out)
    }
}

pub(crate) fn spawn_poller(state: AppState, watcher: Watcher, every: Duration) {
    tokio::spawn(async move {
        let mut watcher = Some(watcher);
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.poked() => {}
            }
            let Some(mut w) = watcher.take() else { break };
            let res = state
                .with_space(move |s| {
                    let ev = w.poll(s);
                    Ok((w, ev))
                })
                .await;
            match res {
                Ok((w, events)) => {
                    watcher = Some(w);
                    match events {
                        Ok(events) => {
                            for e in events {
                                // No subscribers is fine.
                                let _ = state.inner.events.send(e);
                            }
                        }
                        Err(e) => eprintln!("agentvcd: event poll failed: {e}"),
                    }
                }
                Err(e) => {
                    eprintln!("agentvcd: event poller stopped: {e:?}");
                    break;
                }
            }
        }
    });
}

pub(crate) async fn websocket(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream(st, socket))
}

async fn send(socket: &mut WebSocket, e: &Event) -> bool {
    match serde_json::to_string(e) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => true,
    }
}

async fn stream(st: AppState, mut socket: WebSocket) {
    let mut rx = st.subscribe();
    let hello = Event::Hello {
        workdir: st.workdir().to_string_lossy().into_owned(),
    };
    if !send(&mut socket, &hello).await {
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(RecvError::Lagged(_)) => Event::Resync,
                    Err(RecvError::Closed) => break,
                };
                if !send(&mut socket, &ev).await {
                    break;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                // The stream is one-way; ignore anything the client sends.
                Some(Ok(_)) => {}
            }
        }
    }
}
