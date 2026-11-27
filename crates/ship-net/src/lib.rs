//! Ship listeners, ssh `dial-stdio`, client transports and (later) the
//! `tailscaled` local-API client.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3). All tailnet
//! access in Ship lives here.
//!
//! Daemon side: the two listeners ADR-0003 allows before the tailnet
//! arrives, a user/group-only unix socket and a TCP listener on 127.0.0.1.
//! No function here accepts an IP address, so no caller can ask for 0.0.0.0
//! or any other interface.
//!
//! Client side: [`Target`] (`local` or `ssh://`), [`client::get`] for one
//! HTTP exchange over either transport, [`forward::serve`], which carries a
//! 127.0.0.1 port's connections over `ssh … ship dial-stdio` children for
//! `ship open`, and [`dial_stdio`], the host end of the ssh transport. Ship
//! adds no authentication of its own: socket access is the whole
//! authorisation (ADR-0003 rule 2).
//!
//! Tailnet: [`tailnet::TailnetAddress`], the host's tailnet IPv4 address as
//! the local `tailscale` CLI reports it, or none.

pub mod client;
mod connect;
mod dial;
pub mod forward;
mod listener;
pub mod tailnet;
mod target;

pub use connect::{ConnectError, FALLBACK_GROUP, connect_unix, group_name};
pub use dial::{DRAIN_AFTER_INPUT_EOF, bridge, dial_stdio};
pub use listener::{BindError, SOCKET_MODE, SocketFile, bind_loopback, bind_unix};
pub use tailnet::{StaticTailnet, TailnetAddress, TailscaleCli};
pub use target::{DEFAULT_SOCKET, SshTarget, TARGET_FORMS, Target, TargetError};
