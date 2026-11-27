//! Wire protocol between a local daemon (`agentvcd --cloud`) and the hosted
//! service (`agentvc-cloud`).
//!
//! The daemon dials out over one WebSocket, so nothing on the user's machine
//! needs a public port. The hosted UI's API calls travel down that socket as
//! [`CloudMsg::Request`], the daemon answers each with
//! [`DaemonMsg::Response`], and it pushes its live events up as
//! [`DaemonMsg::Event`]. The service keeps no copy of the space: diffs and
//! file contents pass through on demand and are never stored.
//!
//! Every frame is one JSON text message tagged by `t`.

use serde::{Deserialize, Serialize};

/// Bumped on incompatible changes; the service refuses other versions.
pub const PROTOCOL_VERSION: u32 = 1;

/// Path the daemon connects to, relative to the service's URL.
pub const CONNECT_PATH: &str = "/connect";

/// Largest body either side sends in one frame.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Sent by the daemon.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonMsg {
    /// First frame on every connection.
    Hello {
        protocol: u32,
        /// The space's stable id (`agentvc sync`'s), so a reconnect lands
        /// on the same hosted URL.
        space_id: String,
        /// Short display name, the workdir's folder name.
        name: String,
        /// Absolute workdir on the user's machine, shown to its owner only.
        workdir: String,
        /// The daemon's version.
        version: String,
        /// The daemon refuses anything but reads.
        read_only: bool,
    },
    /// The answer to [`CloudMsg::Request`] with the same `id`.
    Response {
        id: u64,
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
        body: String,
    },
    /// One event from the daemon's `/api/events` stream, as is.
    Event { event: serde_json::Value },
}

/// Sent by the service.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum CloudMsg {
    /// Accepted the hello; `url` is where the hosted UI shows this space.
    Welcome { slug: String, url: String },
    /// An API call from the hosted UI. `path` starts with `/api/` and may
    /// carry a query string.
    Request {
        id: u64,
        method: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    /// The service is closing the connection, e.g. a revoked token.
    Error { code: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_tagged_json() {
        let m = CloudMsg::Request {
            id: 7,
            method: "GET".into(),
            path: "/api/tasks".into(),
            content_type: None,
            body: None,
        };
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(
            text,
            r#"{"t":"request","id":7,"method":"GET","path":"/api/tasks"}"#
        );
        assert_eq!(serde_json::from_str::<CloudMsg>(&text).unwrap(), m);
        let ev: DaemonMsg =
            serde_json::from_str(r#"{"t":"event","event":{"type":"resync"}}"#).unwrap();
        assert_eq!(
            ev,
            DaemonMsg::Event {
                event: serde_json::json!({"type": "resync"})
            }
        );
    }
}
