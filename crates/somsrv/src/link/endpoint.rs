//! The two ends of the link.
//!
//! [`SrvLink`] is Som's: it opens channels that read and write messages the way
//! a connection to the daemon does (`read_message`/`write_message`), and
//! carries them to the relay inside the tab's pty stream. [`LinkHub`] is the
//! relay's: for each channel Som opens it makes a real connection to the daemon
//! of its machine and carries the daemon's answers back. Nothing in between
//! looks inside the messages.

use super::frame::{Decoder, Encoder, Message, StreamBroken, split};
use crate::pipe::PipeConnection;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// Writes bytes to the other end: a pty's input (Som) or output (the relay).
pub type SendRaw = Box<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;

/// The frames this end sends, numbered in the order they leave.
struct Outbound {
    encoder: Mutex<Encoder>,
    send: SendRaw,
}

impl Outbound {
    fn new(send: SendRaw) -> Self {
        Self { encoder: Mutex::new(Encoder::default()), send }
    }

    /// Sends a message as one or more frames. Each frame is numbered and
    /// written under one lock, so the numbers are in the order of the writes;
    /// the lock is let go between the frames of a long message, so another
    /// channel's frame can go in between and one big transfer does not hold the
    /// rest up.
    fn send_message(&self, message: &Message) -> Result<()> {
        for part in split(message) {
            let mut encoder = self.encoder.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let frame = encoder.seal(&part);
            (self.send)(&frame).context("cannot write to the pty")?;
        }
        Ok(())
    }
}

// ---- Som's end -----------------------------------------------------------

struct LinkState {
    channels: HashMap<u32, mpsc::Sender<Vec<u8>>>,
}

pub struct SrvLink {
    out: Outbound,
    decoder: Mutex<Decoder>,
    state: Mutex<LinkState>,
    ready: AtomicBool,
    next_channel: AtomicU32,
}

impl SrvLink {
    /// `send` writes bytes into the tab's pty input.
    pub fn new(send: SendRaw) -> Arc<Self> {
        Arc::new(Self {
            out: Outbound::new(send),
            decoder: Mutex::new(Decoder::default()),
            state: Mutex::new(LinkState { channels: HashMap::new() }),
            ready: AtomicBool::new(false),
            next_channel: AtomicU32::new(1),
        })
    }

    /// True once the relay on the other side has said it speaks the link.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Takes in the body of a frame the extractor found in the pty output.
    pub fn on_frame(&self, body: &[u8]) {
        let decoded = self.decoder.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).decode(body);
        match decoded {
            Ok(None) => {},
            Ok(Some(Message::Hello)) => {
                // A relay that says hello again is a new one: what the old one
                // knew of the channels is gone.
                self.drop_channels();
                self.ready.store(true, Ordering::Release);
            },
            Ok(Some(Message::Reset)) => self.drop_channels(),
            Ok(Some(Message::Data { channel, bytes })) => {
                let sender = self.state.lock().unwrap_or_else(|p| p.into_inner()).channels.get(&channel).cloned();
                if let Some(sender) = sender {
                    // A channel nobody reads any more has nothing left to hear.
                    sender.send(bytes).ok();
                }
            },
            Ok(Some(Message::Close { channel })) => {
                self.state.lock().unwrap_or_else(|p| p.into_inner()).channels.remove(&channel);
            },
            Err(broken) => self.recover(broken),
        }
    }

    /// The stream is broken: forget everything on this side, and tell the
    /// relay to do the same, so the channels that are opened next start clean.
    fn recover(&self, broken: StreamBroken) {
        log::warn!("the link to the remote somsrv is broken ({broken:?}): starting over");
        self.decoder.lock().unwrap_or_else(|p| p.into_inner()).reset();
        self.drop_channels();
        if let Err(err) = self.out.send_message(&Message::Reset) {
            log::debug!("cannot tell the relay to reset: {err:#}");
        }
    }

    fn drop_channels(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).channels.clear();
    }

    /// A new connection to the daemon of the remote machine.
    pub fn open_channel(self: &Arc<Self>) -> Result<LinkChannel> {
        anyhow::ensure!(self.is_ready(), "the relay of this tab has not said it speaks the link (an older somsrv, or not started yet)");
        let id = self.next_channel.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.state.lock().unwrap_or_else(|p| p.into_inner()).channels.insert(id, sender);
        Ok(LinkChannel { id, link: self.clone(), incoming: Mutex::new(receiver) })
    }
}

/// One connection to the remote daemon: the same two calls as
/// `PipeConnection`, and the same messages.
pub struct LinkChannel {
    id: u32,
    link: Arc<SrvLink>,
    incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl LinkChannel {
    pub fn read_message(&self) -> Result<Vec<u8>> {
        let incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
        incoming.recv().map_err(|_| anyhow::anyhow!("the remote daemon closed the channel"))
    }

    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        anyhow::ensure!(
            self.link.state.lock().unwrap_or_else(|p| p.into_inner()).channels.contains_key(&self.id),
            "the channel is closed"
        );
        self.link.out.send_message(&Message::Data { channel: self.id, bytes: payload.to_vec() })
    }
}

impl Drop for LinkChannel {
    fn drop(&mut self) {
        let was_open = self.link.state.lock().unwrap_or_else(|p| p.into_inner()).channels.remove(&self.id).is_some();
        if was_open && let Err(err) = self.link.out.send_message(&Message::Close { channel: self.id }) {
            log::debug!("cannot close channel {}: {err:#}", self.id);
        }
    }
}

// ---- the relay's end -------------------------------------------------------

pub struct LinkHub {
    out: Outbound,
    decoder: Mutex<Decoder>,
    channels: Mutex<HashMap<u32, Arc<PipeConnection>>>,
    connect: Box<dyn Fn() -> Result<PipeConnection> + Send + Sync>,
}

impl LinkHub {
    /// `send` writes bytes into the relay's pty output; `connect` reaches the
    /// daemon of this machine.
    pub fn new(send: SendRaw, connect: Box<dyn Fn() -> Result<PipeConnection> + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            out: Outbound::new(send),
            decoder: Mutex::new(Decoder::default()),
            channels: Mutex::new(HashMap::new()),
            connect,
        })
    }

    /// Tells Som this relay speaks the link.
    pub fn announce(&self) -> Result<()> {
        self.out.send_message(&Message::Hello)
    }

    /// Takes in the body of a frame the extractor found in the pty input.
    pub fn on_frame(self: &Arc<Self>, body: &[u8]) {
        let decoded = self.decoder.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).decode(body);
        match decoded {
            Ok(None) | Ok(Some(Message::Hello)) => {},
            Ok(Some(Message::Reset)) => self.close_all(),
            Ok(Some(Message::Data { channel, bytes })) => self.deliver(channel, &bytes),
            Ok(Some(Message::Close { channel })) => {
                let closed = self.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel);
                if let Some(connection) = closed {
                    connection.shutdown();
                }
            },
            Err(broken) => {
                log::warn!("the link from Som is broken ({broken:?}): starting over");
                self.decoder.lock().unwrap_or_else(|p| p.into_inner()).reset();
                self.close_all();
                if let Err(err) = self.out.send_message(&Message::Reset) {
                    log::debug!("cannot tell Som to reset: {err:#}");
                }
            },
        }
    }

    fn close_all(&self) {
        let all: Vec<_> = self.channels.lock().unwrap_or_else(|p| p.into_inner()).drain().map(|(_, connection)| connection).collect();
        for connection in all {
            connection.shutdown();
        }
    }

    fn deliver(self: &Arc<Self>, channel: u32, payload: &[u8]) {
        let existing = self.channels.lock().unwrap_or_else(|p| p.into_inner()).get(&channel).cloned();
        let connection = match existing {
            Some(connection) => connection,
            None => match (self.connect)() {
                Ok(connection) => {
                    let connection = Arc::new(connection);
                    self.channels.lock().unwrap_or_else(|p| p.into_inner()).insert(channel, connection.clone());
                    self.spawn_daemon_reader(channel, connection.clone());
                    connection
                },
                Err(err) => {
                    log::warn!("link channel {channel}: cannot reach the daemon: {err:#}");
                    self.send_close(channel);
                    return;
                },
            },
        };
        if let Err(err) = connection.write_message(payload) {
            log::debug!("link channel {channel}: the daemon went away: {err:#}");
            self.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel);
            connection.shutdown();
            self.send_close(channel);
        }
    }

    fn send_close(&self, channel: u32) {
        if let Err(err) = self.out.send_message(&Message::Close { channel }) {
            log::debug!("link channel {channel}: cannot report the close: {err:#}");
        }
    }

    /// Carries everything the daemon sends on `channel` to Som, and closes the
    /// channel there when the daemon hangs up.
    fn spawn_daemon_reader(self: &Arc<Self>, channel: u32, connection: Arc<PipeConnection>) {
        let hub = self.clone();
        std::thread::spawn(move || {
            while let Ok(message) = connection.read_message() {
                if hub.out.send_message(&Message::Data { channel, bytes: message }).is_err() {
                    break;
                }
            }
            let still_open = hub.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel).is_some();
            if still_open {
                hub.send_close(channel);
            }
        });
    }
}
