//! `consistent-relay` — a rendezvous point so a phone can reach a gateway that
//! is bound to loopback.
//!
//! ### Why not just expose the gateway?
//!
//! Because then every self-hosting operator needs a public IP, a certificate,
//! and a firewall opinion. Here both sides dial *out*: the gateway holds one
//! outbound WebSocket to the relay, the phone makes ordinary HTTPS requests to
//! the relay, and the relay copies bytes between them. Nothing listens on the
//! operator's network, and the gateway's `bind = 127.0.0.1` never changes.
//!
//! ### Trust
//!
//! The relay is a pipe, not an authority. Requests carry the caller's own
//! bearer token, validated only by the gateway. The relay adds exactly two
//! rules of its own:
//!
//! 1. A connection must present the gateway's registration secret to claim its
//!    id, so a stranger cannot impersonate someone's gateway.
//! 2. Static deploy tokens are refused: reaching a machine remotely requires a
//!    real signed-in session, mirroring the gateway's own off-loopback guard.
//!
//! Run it anywhere that can hold a WebSocket (a small VM, Fly, Railway). It
//! keeps no database and no state beyond the live connections.

pub mod protocol;
pub mod server;

pub use protocol::{Frame, Register};
pub use server::{RelayConfig, RelayState, router};
