//! Plugins (`PLUGINS_DESIGN.md`): Lua code that runs here, in the daemon of
//! the tab's host, and draws a panel in Som.
//!
//! [`PluginHost`] holds the running instances. Each [`Instance`] has its own
//! thread with its own Lua VM (`runtime`), a view model (`model`) and at most
//! one attached Som connection, which gets the view and the plugin's `som.*`
//! calls. An instance outlives its connections: Som attaches again after a
//! dropped ssh and gets the whole view as a snapshot.

mod model;
mod runtime;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};

use serde::Deserialize;
use srp_client::plugin::{PLUGIN_API, PluginInfo, SomCall, SomReply, ViewEvent, ViewOp};
use srp_client::protocol::SrvResponse;

use model::Model;

/// Where an attached connection's messages go (`server::send_srv` on it).
pub type Sink = Arc<dyn Fn(&SrvResponse) -> anyhow::Result<()> + Send + Sync>;

/// `plugin.json`, the part of it the daemon uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default = "default_api")]
    pub api: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_entry")]
    pub entry: String,
    /// The programs `proc.spawn` may start.
    #[serde(default)]
    pub spawn: Vec<String>,
    /// The `som.*` calls the plugin may make (`SomCall::permission`).
    #[serde(default)]
    pub permissions: Vec<String>,
}

fn default_api() -> u32 {
    1
}

fn default_entry() -> String {
    "main.lua".into()
}

/// What starts an instance (`SrvRequest::PluginStart`, minus the instance id).
pub struct StartRequest {
    pub manifest: serde_json::Value,
    pub sources: Vec<(String, String)>,
    pub cwd: String,
    pub config: serde_json::Value,
    pub state: Option<serde_json::Value>,
}

#[derive(Clone, Default)]
pub struct PluginHost {
    instances: Arc<Mutex<HashMap<String, Arc<Instance>>>>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no instance runs: the daemon may shut down when idle only then.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Starts `id` and attaches `connection` to it; only attaches if it runs already.
    pub fn start(&self, id: &str, request: StartRequest, connection: u64, sink: Sink) -> anyhow::Result<()> {
        if let Some(instance) = self.get(id) {
            instance.attach(connection, sink);
            return Ok(());
        }
        let manifest: Manifest = serde_json::from_value(request.manifest)?;
        if manifest.api > PLUGIN_API {
            anyhow::bail!(
                "plugin {:?} needs plugin API {}, this somsrv has {PLUGIN_API}",
                manifest.name,
                manifest.api
            );
        }
        let (events, receiver) = mpsc::channel();
        let instance = Arc::new(Instance {
            id: id.to_owned(),
            manifest,
            cwd: request.cwd,
            events,
            shared: Mutex::new(Shared::default()),
        });
        self.lock().insert(id.to_owned(), instance.clone());
        instance.attach(connection, sink);
        let host = self.clone();
        runtime::spawn(instance, request.sources, request.config, request.state, receiver, move |instance, error| {
            host.lock().remove(&instance.id);
            instance.exited(error);
        });
        Ok(())
    }

    pub fn attach(&self, id: &str, connection: u64, sink: Sink) -> anyhow::Result<()> {
        self.require(id)?.attach(connection, sink);
        Ok(())
    }

    pub fn event(&self, id: &str, event: ViewEvent) -> anyhow::Result<()> {
        let instance = self.require(id)?;
        match event {
            ViewEvent::Toggle { id, collapsed } => instance.shared().model.set_collapsed(id, collapsed),
            event => instance.send(runtime::Event::View(event)),
        }
        Ok(())
    }

    pub fn som_reply(&self, id: &str, request: u64, reply: SomReply) -> anyhow::Result<()> {
        self.require(id)?.send(runtime::Event::SomReply { request, reply });
        Ok(())
    }

    pub fn stop(&self, id: &str) -> anyhow::Result<()> {
        self.require(id)?.send(runtime::Event::Stop);
        Ok(())
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        let instances: Vec<Arc<Instance>> = self.lock().values().cloned().collect();
        instances
            .iter()
            .map(|instance| {
                let shared = instance.shared();
                PluginInfo {
                    instance: instance.id.clone(),
                    plugin: instance.manifest.name.clone(),
                    version: instance.manifest.version.clone(),
                    cwd: instance.cwd.clone(),
                    title: shared.model.view.header.title.clone().or_else(|| instance.manifest.title.clone()),
                    status: shared.model.view.header.status.clone(),
                    attached: shared.attached.is_some(),
                }
            })
            .collect()
    }

    /// `connection` is gone: every instance attached to it is detached.
    pub fn detach_connection(&self, connection: u64) {
        let instances: Vec<Arc<Instance>> = self.lock().values().cloned().collect();
        for instance in instances {
            instance.detach_if(connection);
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Instance>> {
        self.lock().get(id).cloned()
    }

    fn require(&self, id: &str) -> anyhow::Result<Arc<Instance>> {
        self.get(id).ok_or_else(|| anyhow::anyhow!("no plugin instance {id}"))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Instance>>> {
        self.instances.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One running plugin instance: what its thread and the connections share.
pub struct Instance {
    id: String,
    manifest: Manifest,
    cwd: String,
    events: mpsc::Sender<runtime::Event>,
    shared: Mutex<Shared>,
}

/// Locked together, so a snapshot and the changes after it never overlap.
#[derive(Default)]
struct Shared {
    model: Model,
    attached: Option<Attached>,
    state: Option<serde_json::Value>,
    /// The changes of a running `ui.batch`: already in `model`, not yet sent.
    batch: Option<Vec<ViewOp>>,
    next_request: u64,
}

struct Attached {
    connection: u64,
    sink: Sink,
}

impl Instance {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send(&self, event: runtime::Event) {
        // The receiver is gone only once the instance's thread has ended,
        // and then nothing is left to tell.
        if self.events.send(event).is_err() {
            log::debug!("plugin instance {} has ended, event dropped", self.id);
        }
    }

    /// Makes `connection` the attached one (replacing any other) and sends
    /// it the whole view and the saved state.
    fn attach(&self, connection: u64, sink: Sink) {
        let mut shared = self.shared();
        // Already in the snapshot.
        if let Some(batch) = shared.batch.as_mut() {
            batch.clear();
        }
        let snapshot = SrvResponse::PluginView {
            instance: self.id.clone(),
            op: ViewOp::Snapshot(shared.model.view.clone()),
        };
        let mut sent = sink(&snapshot);
        if let (Ok(()), Some(state)) = (&sent, shared.state.clone()) {
            sent = sink(&SrvResponse::PluginState { instance: self.id.clone(), state });
        }
        match sent {
            Ok(()) => {
                let replaced = shared.attached.replace(Attached { connection, sink });
                drop(shared);
                // The connection attached before will not answer what the
                // plugin asked it: the plugin sees it go first.
                if replaced.is_some() {
                    self.send(runtime::Event::Detach);
                }
                self.send(runtime::Event::Attach);
            }
            Err(err) => log::warn!("attaching to plugin instance {} failed: {err:#}", self.id),
        }
    }

    fn detach_if(&self, connection: u64) {
        let mut shared = self.shared();
        if shared.attached.as_ref().is_some_and(|attached| attached.connection == connection) {
            shared.attached = None;
            drop(shared);
            self.send(runtime::Event::Detach);
        }
    }

    /// Applies `op` to the model and sends it (or keeps it for the running batch).
    fn view(&self, op: ViewOp) -> anyhow::Result<()> {
        let mut shared = self.shared();
        shared.model.apply(&op)?;
        if let Some(batch) = shared.batch.as_mut() {
            batch.push(op);
            return Ok(());
        }
        self.send_to_attached(&mut shared, SrvResponse::PluginView { instance: self.id.clone(), op });
        Ok(())
    }

    fn begin_batch(&self) -> bool {
        let mut shared = self.shared();
        if shared.batch.is_some() {
            return false; // nested: the outer batch sends it all
        }
        shared.batch = Some(Vec::new());
        true
    }

    fn end_batch(&self) {
        let mut shared = self.shared();
        let Some(ops) = shared.batch.take() else { return };
        if !ops.is_empty() {
            self.send_to_attached(&mut shared, SrvResponse::PluginView { instance: self.id.clone(), op: ViewOp::Batch(ops) });
        }
    }

    /// Sends a `som.*` call to the attached Som; the request id to wait on.
    fn som_request(&self, call: SomCall) -> Result<u64, String> {
        if let Some(permission) = call.permission()
            && !self.manifest.permissions.iter().any(|granted| granted == permission)
        {
            return Err("denied".into());
        }
        let mut shared = self.shared();
        if shared.attached.is_none() {
            return Err("detached".into());
        }
        shared.next_request += 1;
        let request = shared.next_request;
        if self.send_to_attached(&mut shared, SrvResponse::PluginSomRequest { instance: self.id.clone(), request, call }) {
            Ok(request)
        } else {
            Err("detached".into())
        }
    }

    fn save_state(&self, state: serde_json::Value) {
        let mut shared = self.shared();
        shared.state = Some(state.clone());
        self.send_to_attached(&mut shared, SrvResponse::PluginState { instance: self.id.clone(), state });
    }

    /// Sends `message` to the attached connection, if any; a connection that
    /// fails is detached. Whether it was sent.
    fn send_to_attached(&self, shared: &mut Shared, message: SrvResponse) -> bool {
        let Some(attached) = shared.attached.as_ref() else { return false };
        match (attached.sink)(&message) {
            Ok(()) => true,
            Err(err) => {
                log::info!("plugin instance {}: attached connection failed ({err:#}), detaching", self.id);
                shared.attached = None;
                self.send(runtime::Event::Detach);
                false
            }
        }
    }

    fn exited(&self, error: Option<String>) {
        let mut shared = self.shared();
        self.send_to_attached(&mut shared, SrvResponse::PluginExited { instance: self.id.clone(), error });
        shared.attached = None;
    }
}
