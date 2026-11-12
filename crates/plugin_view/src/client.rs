//! Som's end of one plugin instance: a connection to the daemon of the
//! tab's machine (through the tab's link for a remote tab), `PluginStart` on
//! it, and a reader thread turning the daemon's messages into [`Incoming`].

use std::sync::{Arc, Mutex};

use somsrv::link::SrvLink;
use somsrv::protocol::{SrvRequest, SrvResponse};
use srp_client::plugin::{SomCall, SomReply, ViewEvent, ViewOp};
use terminal::rich_content_srv_channel::{SrvConnection, connect};

use crate::loader::Plugin;

pub enum Incoming {
    View(ViewOp),
    SomRequest { request: u64, call: SomCall },
    State(serde_json::Value),
    /// The instance ended (`error` when it failed).
    Exited(Option<String>),
    /// The connection is gone (ssh dropped, the daemon went away); the
    /// instance may still run on its machine.
    Disconnected(String),
}

pub struct PluginClient {
    instance: String,
    connection: SrvConnection,
    /// One message at a time: a named pipe write is two writes (length, body).
    writer: Mutex<()>,
}

impl PluginClient {
    /// Connects and starts the instance. Blocking (it may start this
    /// machine's daemon): run it off the UI thread.
    pub fn start(
        link: Option<Arc<SrvLink>>,
        instance: String,
        plugin: &Plugin,
        cwd: String,
        config: serde_json::Value,
        state: Option<serde_json::Value>,
    ) -> anyhow::Result<(Arc<Self>, async_channel::Receiver<Incoming>)> {
        let connection = connect(link)?;
        let client = Arc::new(Self { instance: instance.clone(), connection, writer: Mutex::new(()) });
        client.send(&SrvRequest::PluginStart {
            instance,
            manifest: plugin.manifest.clone(),
            sources: plugin.sources.clone(),
            cwd,
            config,
            state,
        })?;
        let (sender, receiver) = async_channel::unbounded();
        let reader = client.clone();
        std::thread::Builder::new().name("plugin-client".into()).spawn(move || reader.read_loop(sender))?;
        Ok((client, receiver))
    }

    pub fn event(&self, event: ViewEvent) -> anyhow::Result<()> {
        self.send(&SrvRequest::PluginEvent { instance: self.instance.clone(), event })
    }

    pub fn reply(&self, request: u64, reply: SomReply) -> anyhow::Result<()> {
        self.send(&SrvRequest::PluginSomReply { instance: self.instance.clone(), request, reply })
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        self.send(&SrvRequest::PluginStop { instance: self.instance.clone() })
    }

    fn send(&self, message: &SrvRequest) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(message)?;
        let _guard = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.connection.write_message(&payload)
    }

    fn read_loop(&self, sender: async_channel::Sender<Incoming>) {
        loop {
            let message = match self.connection.read_message() {
                Ok(bytes) => match serde_json::from_slice::<SrvResponse>(&bytes) {
                    Ok(message) => message,
                    Err(err) => {
                        log::warn!("plugin client: unreadable message from somsrv: {err}");
                        continue;
                    }
                },
                Err(err) => {
                    if sender.send_blocking(Incoming::Disconnected(format!("{err:#}"))).is_err() {
                        log::debug!("plugin view gone before its connection");
                    }
                    return;
                }
            };
            let incoming = match message {
                SrvResponse::PluginView { instance, op } if instance == self.instance => Incoming::View(op),
                SrvResponse::PluginSomRequest { instance, request, call } if instance == self.instance => {
                    Incoming::SomRequest { request, call }
                }
                SrvResponse::PluginState { instance, state } if instance == self.instance => Incoming::State(state),
                SrvResponse::PluginExited { instance, error } if instance == self.instance => Incoming::Exited(error),
                _ => continue,
            };
            let exited = matches!(incoming, Incoming::Exited(_));
            if sender.send_blocking(incoming).is_err() || exited {
                return;
            }
        }
    }
}
