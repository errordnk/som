//! The client side of Som's rich-content protocol (SRP): everything a program
//! needs to show a picture, a video, audio or a Markdown document in Som,
//! without depending on Som itself.
//!
//! - [`protocol`]: the JSON messages of `somsrv`'s binary side channel (the
//!   daemon's own types: `somsrv` re-exports them, so there is one definition).
//! - [`pipe`]: the length-prefixed framing over a named pipe (Windows) or a
//!   Unix domain socket.
//! - [`placeholder`]: the placeholder cells a placement is drawn with.
//! - [`channel`]: a client connection to a running daemon.
//! - [`cue`]: `.cue` sheets.
//!
//! `SRP_INTEGRATION_GUIDE.md` in the som repository describes the protocol.

pub mod channel;
pub mod cue;
pub mod pipe;
pub mod placeholder;
pub mod plugin;
pub mod protocol;
