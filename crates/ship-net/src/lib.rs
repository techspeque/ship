//! Ship listeners, ssh `dial-stdio` and the `tailscaled` local-API client.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3). All tailnet
//! access in Ship lives here.
//!
//! Phase 0 provides the two daemon listeners ADR-0003 allows before the
//! tailnet arrives: a user/group-only unix socket and a TCP listener on
//! 127.0.0.1. No function here accepts an IP address, so no caller can ask
//! for 0.0.0.0 or any other interface.

mod listener;

pub use listener::{BindError, SOCKET_MODE, SocketFile, bind_loopback, bind_unix};
