//! Public surface shared between the `som-srv` binary and Som's own
//! client-side integration. Only the transport layer is exposed here —
//! `bounds`/`session`/`server` (the PTY/session-holding internals) stay
//! private to the binary; Som only ever talks to a running server over the
//! pipe protocol, it never links against session/PTY internals directly.
pub mod admin;
pub mod daemon;
pub mod pipe;
pub mod protocol;
