//! The link: Som's channel to the `somsrv` daemon of a remote machine, carried
//! inside the pty stream of the tab that already runs on that machine - the
//! same ssh (or wsl) connection, nothing else started, nothing listening on
//! any port, no assumption about the network between the two.
//!
//! - [`frame`]: the frames, and taking them out of a byte stream.
//! - [`vt`] and [`mux`]: writing them into the relay's output without
//!   breaking the shell's own escape sequences.
//! - [`endpoint`]: Som's end ([`SrvLink`]) and the relay's ([`LinkHub`]).

mod endpoint;
mod frame;
mod mux;
mod vt;

pub use endpoint::{LinkChannel, LinkHub, SendRaw, SrvLink};
pub use frame::{Extractor, Framing};
pub use mux::PtyOutput;
