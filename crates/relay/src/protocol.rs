//! The frames a gateway and the relay exchange over one WebSocket.
//!
//! Deliberately boring: JSON text frames, one multiplexed connection, request
//! ids for correlation. Bodies ride base64-encoded inside the frames rather
//! than as binary WS messages, so a single ordered stream carries everything
//! and there is no second channel to keep in sync.
//!
//! ### What the relay is, and is not
//!
//! It is a rendezvous point: the gateway dials *out* to it, the phone talks to
//! it, and it copies bytes between the two. It is **not** an authority. Every
//! forwarded request still carries the caller's own bearer token, which only
//! the gateway can validate — so a compromised relay can disrupt traffic but
//! cannot mint access to anyone's data.

use serde::{Deserialize, Serialize};

/// Gateway → relay, first frame on a new connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Register {
    /// Stable public id for this gateway; forms the phone's URL prefix.
    pub gateway_id: String,
    /// Proves this connection may claim that id. Never leaves the operator's
    /// two machines and the relay they chose.
    pub secret: String,
}

/// Every message on the socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Frame {
    /// Gateway → relay: claim a gateway id.
    Register(Register),
    /// Relay → gateway: registration accepted.
    Registered {
        gateway_id: String,
    },
    /// Relay → gateway: a request to replay against local loopback.
    Request {
        id: u64,
        method: String,
        /// Path plus query, e.g. `/api/tasks?limit=20`.
        path: String,
        headers: Vec<(String, String)>,
        /// base64, empty when there is no body.
        #[serde(default)]
        body: String,
    },
    /// Gateway → relay: response head. Chunks follow until `End`.
    Response {
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Gateway → relay: one body chunk (base64). Streaming responses (NDJSON
    /// chat) arrive as many of these.
    Chunk {
        id: u64,
        data: String,
    },
    /// Gateway → relay: the response is complete.
    End {
        id: u64,
    },
    /// Either direction: this request failed locally.
    Failed {
        id: u64,
        message: String,
    },
    /// Keepalive both ways.
    Ping,
    Pong,
}

impl Frame {
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn decode(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }
}

/// Headers the relay must never copy from the phone's request into the
/// replayed one: hop-by-hop headers, and anything about the *relay's* own
/// transport rather than the caller's intent.
pub const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

pub fn is_stripped_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    STRIPPED_REQUEST_HEADERS.iter().any(|h| *h == lower)
}

/// Header the relay stamps on every forwarded request so the gateway knows the
/// call arrived from outside the machine.
///
/// The relay cannot tell a session token from a deploy token — the gateway
/// mints both with the same generator (64 hex characters, no structure), so
/// *any* shape-based guess is wrong. An earlier version of this file guessed
/// anyway and got it exactly backwards: it rejected every real session, and
/// would have admitted the one dotted form (`<secret>.<workspace>.<principal>`)
/// that actually *is* the deploy token's impersonation syntax.
///
/// So the classification moves to the only party that can make it: the gateway,
/// which resolves the bearer against its own store. The relay's job is to say
/// truthfully "this came in over the tunnel"; the gateway refuses static
/// principals on such requests.
///
/// This marker is authoritative because the relay *strips any client-supplied
/// copy* before setting its own ([`strip_client_relay_marker`]). It can only
/// tighten what a request may do, never loosen it — a local process that sets
/// it by hand merely restricts itself.
pub const RELAY_MARKER_HEADER: &str = "x-consistent-relay";

/// Remove any caller-supplied relay marker, in any casing. Called before the
/// relay adds its own so the marker reflects reality, not the caller's wishes.
pub fn strip_client_relay_marker(headers: &mut Vec<(String, String)>) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case(RELAY_MARKER_HEADER));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let cases = vec![
            Frame::Register(Register {
                gateway_id: "gw-1".into(),
                secret: "s".into(),
            }),
            Frame::Registered {
                gateway_id: "gw-1".into(),
            },
            Frame::Request {
                id: 7,
                method: "POST".into(),
                path: "/api/tasks?limit=2".into(),
                headers: vec![("authorization".into(), "Bearer x.y.z".into())],
                body: "aGk=".into(),
            },
            Frame::Response {
                id: 7,
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
            },
            Frame::Chunk {
                id: 7,
                data: "aGk=".into(),
            },
            Frame::End { id: 7 },
            Frame::Failed {
                id: 7,
                message: "boom".into(),
            },
            Frame::Ping,
            Frame::Pong,
        ];
        for frame in cases {
            let encoded = frame.encode().expect("encode");
            let decoded = Frame::decode(&encoded).expect("decode");
            assert_eq!(frame, decoded);
        }
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        for h in ["Host", "connection", "Content-Length", "Upgrade"] {
            assert!(is_stripped_request_header(h), "{h} must be stripped");
        }
        for h in ["authorization", "x-workspace-id", "content-type"] {
            assert!(!is_stripped_request_header(h), "{h} must survive");
        }
    }

    #[test]
    fn a_client_supplied_relay_marker_is_stripped_so_it_cannot_be_forged() {
        let mut headers = vec![
            ("authorization".to_string(), "Bearer abc".to_string()),
            ("X-Consistent-Relay".to_string(), "1".to_string()),
            ("x-consistent-relay".to_string(), "spoofed".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        strip_client_relay_marker(&mut headers);
        assert!(
            !headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(RELAY_MARKER_HEADER)),
            "every casing of the marker must be removed"
        );
        // Everything else survives untouched.
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|(n, _)| n == "authorization"));
    }
}
