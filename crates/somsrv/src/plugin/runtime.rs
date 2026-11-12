//! The thread of one plugin instance: its Lua VM, its event queue, its
//! processes and timers, and the Lua API (`PLUGINS_DESIGN.md` section 6).
//!
//! Every handler (`plugin.on(...)`) runs as a coroutine. A `som.*` call
//! sends its request and yields; the coroutine is resumed when Som answers
//! (or with `detached` when Som goes away), and other events are handled
//! meanwhile. A handler that runs longer than [`HANDLER_TIME_LIMIT`] without
//! yielding is stopped with an error; the instance goes on.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use mlua::{
    AnyUserData, Function, HookTriggers, IntoLuaMulti as _, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table, Thread, ThreadStatus,
    UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use srp_client::plugin::{Block, Header, Input, SomCall, SomReply, ViewEvent, ViewOp};

use super::Instance;

/// The longest a handler may run without yielding.
pub const HANDLER_TIME_LIMIT: Duration = Duration::from_secs(5);

/// How often (in Lua instructions) the time limit is checked.
const HOOK_EVERY: u32 = 10_000;

/// The yield marker of a `som.*` call (see the prelude).
const SOM_WAIT: &str = "__som_wait";

/// The `som.*` functions this build has (`som.has`).
const SOM_FUNCTIONS: &[&str] = &["open_terminal", "notify", "preview", "clipboard_write", "confirm", "open_url", "open_plugin"];

pub enum Event {
    Attach,
    Detach,
    View(ViewEvent),
    SomReply { request: u64, reply: SomReply },
    Stop,
    Proc { id: u64, event: ProcEvent },
}

pub enum ProcEvent {
    Line(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(Option<i32>),
}

/// Starts the instance's thread. `on_exit` runs on it at the end, with the
/// error that ended the instance, if any.
pub fn spawn(
    instance: Arc<Instance>,
    sources: Vec<(String, String)>,
    config: serde_json::Value,
    state: Option<serde_json::Value>,
    events: mpsc::Receiver<Event>,
    on_exit: impl FnOnce(&Arc<Instance>, Option<String>) + Send + 'static,
) {
    let name = format!("plugin-{}", instance.manifest.name);
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let error = match Runtime::new(instance.clone()) {
            Ok(runtime) => runtime.run(sources, config, state, events).err(),
            Err(err) => Some(err),
        };
        let error = error.map(|err| format!("{err:#}"));
        if let Some(error) = &error {
            log::warn!("plugin instance {} ({}) ended: {error}", instance.id, instance.manifest.name);
        }
        on_exit(&instance, error);
    });
    if let Err(err) = spawned {
        log::error!("could not start a plugin thread: {err}");
    }
}

/// What the Lua API functions share with the event loop.
struct Inner {
    procs: HashMap<u64, Proc>,
    next_proc: u64,
    timers: HashMap<String, Timer>,
    /// The environment of the user's login shell, read at the first
    /// `proc.spawn` (`None` until then; `Some(None)` if it could not be read).
    login_env: Option<Option<HashMap<String, String>>>,
}

struct Timer {
    due: Instant,
    every: Option<Duration>,
}

struct Runtime {
    lua: Lua,
    instance: Arc<Instance>,
    inner: Arc<Mutex<Inner>>,
    /// Coroutines waiting for Som's answer, by request id.
    waiting: HashMap<u64, Thread>,
    deadline: Arc<Mutex<Instant>>,
    stopping: bool,
}

impl Runtime {
    fn new(instance: Arc<Instance>) -> anyhow::Result<Self> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
            LuaOptions::default(),
        )?;
        let inner = Arc::new(Mutex::new(Inner { procs: HashMap::new(), next_proc: 0, timers: HashMap::new(), login_env: None }));
        let runtime = Self {
            lua,
            instance,
            inner,
            waiting: HashMap::new(),
            deadline: Arc::new(Mutex::new(Instant::now())),
            stopping: false,
        };
        Ok(runtime)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn run(
        mut self,
        sources: Vec<(String, String)>,
        config: serde_json::Value,
        state: Option<serde_json::Value>,
        events: mpsc::Receiver<Event>,
    ) -> anyhow::Result<()> {
        self.install_api(sources)?;
        let entry = self.instance.manifest.entry.trim_end_matches(".lua").replace('/', ".");
        let require: Function = self.lua.globals().get("require")?;
        self.with_deadline(|| require.call::<Value>(entry))?;

        let ctx = self.lua.create_table()?;
        ctx.set("cwd", self.instance.cwd.clone())?;
        ctx.set("config", self.lua.to_value(&config)?)?;
        if let Some(state) = state {
            ctx.set("state", self.lua.to_value(&state)?)?;
            self.instance.shared().state = Some(state);
        }
        self.dispatch("start", MultiValue::from_iter([Value::Table(ctx)]));

        while !self.stopping {
            let event = match self.next_timer() {
                Some(due) => match events.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match events.recv() {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvError) => break,
                },
            };
            if let Some(event) = event {
                self.handle(event)?;
            }
            self.fire_timers();
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> anyhow::Result<()> {
        match event {
            Event::Attach => self.dispatch("attach", MultiValue::new()),
            Event::Detach => {
                for (_, thread) in std::mem::take(&mut self.waiting) {
                    self.resume("som", thread, (false, "detached"));
                }
                self.dispatch("detach", MultiValue::new());
            }
            Event::View(ViewEvent::Action { block_id, button_id }) => {
                self.dispatch("action", (block_id, button_id).into_lua_multi(&self.lua)?)
            }
            Event::View(ViewEvent::Submit { text }) => self.dispatch("submit", (text,).into_lua_multi(&self.lua)?),
            Event::View(ViewEvent::Interrupt) => self.dispatch("interrupt", MultiValue::new()),
            Event::View(ViewEvent::Command { name }) => self.dispatch("command", (name,).into_lua_multi(&self.lua)?),
            Event::View(ViewEvent::Toggle { .. }) => {} // kept in the model by the host
            Event::SomReply { request, reply } => {
                if let Some(thread) = self.waiting.remove(&request) {
                    match reply {
                        SomReply::Ok(value) => {
                            let value = self.lua.to_value(&value)?;
                            self.resume("som", thread, (true, value));
                        }
                        SomReply::Err(error) => self.resume("som", thread, (false, error)),
                    }
                }
            }
            Event::Stop => {
                self.dispatch("stop", MultiValue::new());
                self.stopping = true;
            }
            Event::Proc { id, event } => {
                let Some(handle) = self.inner().procs.get(&id).map(|proc| proc.handle.clone()) else {
                    return Ok(());
                };
                let handle = Value::UserData(handle);
                match event {
                    ProcEvent::Line(bytes) => {
                        self.dispatch("line", (handle, self.lua.create_string(bytes)?).into_lua_multi(&self.lua)?)
                    }
                    ProcEvent::Stdout(bytes) => {
                        self.dispatch("stdout", (handle, self.lua.create_string(bytes)?).into_lua_multi(&self.lua)?)
                    }
                    ProcEvent::Stderr(bytes) => {
                        self.dispatch("stderr", (handle, self.lua.create_string(bytes)?).into_lua_multi(&self.lua)?)
                    }
                    ProcEvent::Exit(code) => {
                        self.inner().procs.remove(&id);
                        self.dispatch("exit", (handle, code).into_lua_multi(&self.lua)?);
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs the handler for `name`, if the plugin has one, as a new coroutine.
    fn dispatch(&mut self, name: &str, args: MultiValue) {
        let handler = self
            .lua
            .globals()
            .get::<Function>("__plugin_handler")
            .and_then(|lookup| lookup.call::<Option<Function>>(name));
        let thread = match handler {
            Ok(Some(handler)) => self.lua.create_thread(handler),
            Ok(None) => return,
            Err(err) => Err(err),
        };
        match thread {
            Ok(thread) => self.resume(name, thread, args),
            Err(err) => self.report(name, &err),
        }
    }

    fn resume(&mut self, name: &str, thread: Thread, args: impl mlua::IntoLuaMulti) {
        let deadline = self.deadline.clone();
        *lock(&deadline) = Instant::now() + HANDLER_TIME_LIMIT;
        thread.set_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
            if Instant::now() > *lock(&deadline) {
                return Err(mlua::Error::runtime(format!(
                    "the handler ran longer than {}s without yielding",
                    HANDLER_TIME_LIMIT.as_secs()
                )));
            }
            Ok(VmState::Continue)
        });
        match thread.resume::<MultiValue>(args) {
            Ok(values) => {
                if thread.status() != ThreadStatus::Resumable {
                    return;
                }
                let mut values = values.into_iter();
                let marker = values.next();
                let request = values.next();
                match (marker, request) {
                    (Some(Value::String(marker)), Some(Value::Integer(request))) if marker.to_str().is_ok_and(|m| m == SOM_WAIT) => {
                        self.waiting.insert(request as u64, thread);
                    }
                    _ => self.report(
                        name,
                        &mlua::Error::runtime("a handler may only yield inside som.* calls (use your own coroutines inside it)"),
                    ),
                }
            }
            Err(err) => self.report(name, &err),
        }
    }

    /// Runs `f` (plain Lua, not a handler) under the time limit.
    fn with_deadline<R>(&self, f: impl FnOnce() -> mlua::Result<R>) -> anyhow::Result<R> {
        let deadline = self.deadline.clone();
        *lock(&deadline) = Instant::now() + HANDLER_TIME_LIMIT;
        self.lua.set_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
            if Instant::now() > *lock(&deadline) {
                return Err(mlua::Error::runtime("loading the plugin took too long"));
            }
            Ok(VmState::Continue)
        });
        let result = f();
        self.lua.remove_hook();
        Ok(result?)
    }

    /// A failed handler: logged, and shown in the panel's header.
    fn report(&self, handler: &str, err: &mlua::Error) {
        let message = format!("{handler}: {err}");
        log::warn!("plugin instance {} ({}): {message}", self.instance.id, self.instance.manifest.name);
        let header = Header { error: Some(message), ..self.instance.shared().model.view.header.clone() };
        if let Err(err) = self.instance.view(ViewOp::Header(header)) {
            log::warn!("could not show a plugin error: {err:#}");
        }
    }

    fn next_timer(&self) -> Option<Instant> {
        self.inner().timers.values().map(|timer| timer.due).min()
    }

    fn fire_timers(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = {
            let mut inner = self.inner();
            let names: Vec<String> =
                inner.timers.iter().filter(|(_, timer)| timer.due <= now).map(|(name, _)| name.clone()).collect();
            for name in &names {
                let repeat = inner.timers.get(name).and_then(|timer| timer.every);
                match repeat {
                    Some(every) => {
                        if let Some(timer) = inner.timers.get_mut(name) {
                            timer.due = now + every;
                        }
                    }
                    None => {
                        inner.timers.remove(name);
                    }
                }
            }
            names
        };
        for name in due {
            match (name.clone(),).into_lua_multi(&self.lua) {
                Ok(args) => self.dispatch("timer", args),
                Err(err) => self.report("timer", &err),
            }
        }
    }

    fn install_api(&self, sources: Vec<(String, String)>) -> anyhow::Result<()> {
        let lua = &self.lua;
        let globals = lua.globals();

        // The base library reads files (`dofile`, `loadfile`) and loads
        // bytecode (`load` with mode "b"); none of that is for plugins.
        globals.set("dofile", Value::Nil)?;
        globals.set("loadfile", Value::Nil)?;
        let load: Function = globals.get("load")?;
        globals.set("__load", load)?;

        let source_table = lua.create_table()?;
        for (path, text) in sources {
            source_table.set(path.replace('\\', "/"), text)?;
        }
        globals.set("__sources", source_table)?;

        let plugin_name = self.instance.manifest.name.clone();
        let log_table = lua.create_table()?;
        for (level, name) in [(log::Level::Info, "info"), (log::Level::Warn, "warn"), (log::Level::Error, "error")] {
            let plugin_name = plugin_name.clone();
            log_table.set(
                name,
                lua.create_function(move |_, args: MultiValue| {
                    let text: Vec<String> = args.iter().map(|value| value.to_string().unwrap_or_default()).collect();
                    log::log!(level, "plugin {plugin_name}: {}", text.join(" "));
                    Ok(())
                })?,
            )?;
        }
        globals.set("log", log_table)?;

        globals.set("ui", self.ui_table()?)?;
        globals.set("proc", self.proc_table()?)?;
        globals.set("timer", self.timer_table()?)?;
        globals.set("json", json_table(lua)?)?;
        globals.set("host", host_table(lua)?)?;

        let time = lua.create_table()?;
        time.set(
            "now",
            lua.create_function(|_, ()| {
                Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0))
            })?,
        )?;
        globals.set("time", time)?;

        let state = lua.create_table()?;
        let instance = self.instance.clone();
        state.set(
            "save",
            lua.create_function(move |lua, value: Value| {
                let value: serde_json::Value = lua.from_value(value)?;
                instance.save_state(value);
                Ok(())
            })?,
        )?;
        globals.set("state", state)?;

        let instance = self.instance.clone();
        globals.set(
            "__som_send",
            lua.create_function(move |lua, call: Value| {
                let call: SomCall = lua.from_value(call)?;
                Ok(match instance.som_request(call) {
                    Ok(request) => (Some(request), None),
                    Err(error) => (None, Some(error)),
                })
            })?,
        )?;
        let has = lua.create_table()?;
        for name in SOM_FUNCTIONS {
            has.set(*name, true)?;
        }
        globals.set("__som_has", has)?;
        globals.set("__som_api", srp_client::plugin::PLUGIN_API)?;

        lua.load(PRELUDE).set_name("=prelude").exec()?;
        Ok(())
    }

    fn ui_table(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let ui = lua.create_table()?;

        let instance = self.instance.clone();
        ui.set(
            "header",
            lua.create_function(move |lua, fields: Table| {
                let mut header = instance.shared().model.view.header.clone();
                merge_string(lua, &fields, "title", &mut header.title)?;
                merge_string(lua, &fields, "subtitle", &mut header.subtitle)?;
                merge_string(lua, &fields, "status", &mut header.status)?;
                view(&instance, ViewOp::Header(header))
            })?,
        )?;

        let instance = self.instance.clone();
        ui.set(
            "input",
            lua.create_function(move |_, fields: Table| {
                let mut input: Input = instance.shared().model.view.input.clone();
                if let Some(enabled) = fields.get::<Option<bool>>("enabled")? {
                    input.enabled = enabled;
                }
                if let Some(multiline) = fields.get::<Option<bool>>("multiline")? {
                    input.multiline = multiline;
                }
                if fields.contains_key("placeholder")? {
                    input.placeholder = fields.get("placeholder")?;
                }
                view(&instance, ViewOp::Input(input))
            })?,
        )?;

        let instance = self.instance.clone();
        ui.set(
            "append",
            lua.create_function(move |lua, fields: Table| {
                let kind: String = fields.get("kind")?;
                let props: Value = fields.get("props")?;
                let props: serde_json::Value = match props {
                    Value::Nil => serde_json::Value::Object(serde_json::Map::new()),
                    props => lua.from_value(props)?,
                };
                let parent: Option<u64> = fields.get("parent")?;
                let collapsed = fields.get::<Option<bool>>("collapsed")?.unwrap_or(false);
                let id = instance.shared().model.next_block_id();
                view(&instance, ViewOp::Append { parent, block: Block { id, kind, props, collapsed, children: Vec::new() } })?;
                Ok(id)
            })?,
        )?;

        let instance = self.instance.clone();
        ui.set(
            "update",
            lua.create_function(move |lua, (id, props): (u64, Value)| {
                let props: serde_json::Map<String, serde_json::Value> = lua.from_value(props)?;
                view(&instance, ViewOp::Update { id, props })
            })?,
        )?;

        let instance = self.instance.clone();
        ui.set("remove", lua.create_function(move |_, id: u64| view(&instance, ViewOp::Remove { id }))?)?;
        let instance = self.instance.clone();
        ui.set("clear", lua.create_function(move |_, ()| view(&instance, ViewOp::Clear))?)?;
        let instance = self.instance.clone();
        ui.set("scroll_to", lua.create_function(move |_, id: u64| view(&instance, ViewOp::ScrollTo { id }))?)?;

        let instance = self.instance.clone();
        ui.set(
            "batch",
            lua.create_function(move |_, f: Function| {
                let outer = instance.begin_batch();
                let result = f.call::<MultiValue>(());
                if outer {
                    instance.end_batch();
                }
                result
            })?,
        )?;
        Ok(ui)
    }

    fn proc_table(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let table = lua.create_table()?;
        let instance = self.instance.clone();
        let inner = self.inner.clone();
        table.set(
            "spawn",
            lua.create_function(move |lua, fields: Table| {
                let program: String = fields.get("program")?;
                if !instance.manifest.spawn.contains(&program) {
                    return Err(mlua::Error::runtime(format!("{program:?} is not in the manifest's \"spawn\" list")));
                }
                let args: Vec<String> = fields.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
                let cwd: String = fields.get::<Option<String>>("cwd")?.unwrap_or_else(|| instance.cwd.clone());
                let env: HashMap<String, String> = fields.get::<Option<HashMap<String, String>>>("env")?.unwrap_or_default();
                let lines = match fields.get::<Option<String>>("mode")?.as_deref() {
                    None | Some("lines") => true,
                    Some("bytes") => false,
                    Some(other) => return Err(mlua::Error::runtime(format!("unknown proc mode {other:?}"))),
                };
                let id = {
                    let mut inner = lock(&inner);
                    inner.next_proc += 1;
                    inner.next_proc
                };
                let base_env = {
                    let mut inner = lock(&inner);
                    inner.login_env.get_or_insert_with(login_environment).clone()
                };
                let started = start_process(&instance, id, &program, &args, &cwd, base_env.as_ref(), &env, lines)
                    .map_err(|err| mlua::Error::runtime(format!("starting {program:?}: {err:#}")))?;
                let handle = lua.create_userdata(ProcHandle { id, pid: started.pid, inner: inner.clone() })?;
                lock(&inner).procs.insert(id, Proc { child: started.child, stdin: started.stdin, handle: handle.clone() });
                Ok(handle)
            })?,
        )?;
        Ok(table)
    }

    fn timer_table(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let table = lua.create_table()?;
        for (name, repeat) in [("after", false), ("every", true)] {
            let inner = self.inner.clone();
            table.set(
                name,
                lua.create_function(move |_, (ms, name): (u64, String)| {
                    let period = Duration::from_millis(ms);
                    let timer = Timer { due: Instant::now() + period, every: repeat.then_some(period) };
                    lock(&inner).timers.insert(name, timer);
                    Ok(())
                })?,
            )?;
        }
        let inner = self.inner.clone();
        table.set(
            "cancel",
            lua.create_function(move |_, name: String| {
                lock(&inner).timers.remove(&name);
                Ok(())
            })?,
        )?;
        Ok(table)
    }
}

impl Drop for Runtime {
    /// The instance is over: its processes go with it.
    fn drop(&mut self) {
        self.waiting.clear();
        let procs: Vec<Proc> = self.inner().procs.drain().map(|(_, proc)| proc).collect();
        for proc in procs {
            if let Err(err) = lock(&proc.child).kill() {
                log::debug!("killing a plugin process: {err}");
            }
        }
    }
}

fn view(instance: &Instance, op: ViewOp) -> mlua::Result<()> {
    instance.view(op).map_err(|err| mlua::Error::runtime(format!("{err:#}")))
}

/// Sets `target` from `fields[key]` when the key is there (`json.null`/`false` clears it).
fn merge_string(lua: &Lua, fields: &Table, key: &str, target: &mut Option<String>) -> mlua::Result<()> {
    let value: Value = fields.get(key)?;
    match value {
        Value::Nil => {}
        Value::Boolean(false) => *target = None,
        value if value == lua.null() => *target = None,
        value => *target = Some(<String as mlua::FromLua>::from_lua(value, lua)?),
    }
    Ok(())
}

fn json_table(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })?,
    )?;
    table.set(
        "decode",
        lua.create_function(|lua, text: mlua::String| {
            let value: serde_json::Value = serde_json::from_slice(&text.as_bytes()).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })?,
    )?;
    table.set("null", lua.null())?;
    table.set("array_mt", lua.array_metatable())?;
    Ok(table)
}

fn host_table(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("os", lua.create_function(|_, ()| Ok(std::env::consts::OS))?)?;
    table.set("name", lua.create_function(|_, ()| Ok(sysinfo::System::host_name()))?)?;
    table.set(
        "home",
        lua.create_function(|_, ()| Ok(std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()))?,
    )?;
    table.set("env", lua.create_function(|_, name: String| Ok(std::env::var(name).ok()))?)?;
    Ok(table)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A process a plugin started.
struct Proc {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    handle: AnyUserData,
}

/// The Lua side of a process: `p:write`, `p:close_stdin`, `p:kill`, `p.pid`.
struct ProcHandle {
    id: u64,
    pid: u32,
    inner: Arc<Mutex<Inner>>,
}

impl UserData for ProcHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("pid", |_, this| Ok(this.pid));
        fields.add_field_method_get("id", |_, this| Ok(this.id));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("write", |_, this, data: mlua::String| {
            let mut inner = lock(&this.inner);
            let stdin = inner
                .procs
                .get_mut(&this.id)
                .and_then(|proc| proc.stdin.as_mut())
                .ok_or_else(|| mlua::Error::runtime("the process's stdin is closed"))?;
            stdin.write_all(&data.as_bytes()).and_then(|()| stdin.flush()).map_err(mlua::Error::external)
        });
        methods.add_method("close_stdin", |_, this, ()| {
            if let Some(proc) = lock(&this.inner).procs.get_mut(&this.id) {
                proc.stdin = None;
            }
            Ok(())
        });
        methods.add_method("kill", |_, this, ()| {
            let child = lock(&this.inner).procs.get(&this.id).map(|proc| proc.child.clone());
            if let Some(child) = child {
                lock(&child).kill().map_err(mlua::Error::external)?;
            }
            Ok(())
        });
    }
}

struct Started {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    pid: u32,
}

/// Starts a process with its readers: stdout (by lines or chunks) and stderr
/// become events, and so does its exit, after the last of its output.
fn start_process(
    instance: &Arc<Instance>,
    id: u64,
    program: &str,
    args: &[String],
    cwd: &str,
    base_env: Option<&HashMap<String, String>>,
    env: &HashMap<String, String>,
    lines: bool,
) -> anyhow::Result<Started> {
    let mut command = Command::new(program);
    // The login shell's environment, when there is one, in place of the
    // daemon's: its PATH also decides where `program` is found.
    if let Some(base_env) = base_env {
        command.env_clear().envs(base_env);
    }
    command.args(args).current_dir(cwd).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("no stderr"))?;
    let child = Arc::new(Mutex::new(child));

    let sender = instance.events.clone();
    let stdout_reader = std::thread::spawn(move || {
        if lines {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        while line.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
                            line.pop();
                        }
                        if sender.send(Event::Proc { id, event: ProcEvent::Line(line.clone()) }).is_err() {
                            return;
                        }
                    }
                }
            }
        } else {
            forward_chunks(stdout, &sender, id, ProcEvent::Stdout);
        }
    });
    let sender = instance.events.clone();
    let stderr_reader = std::thread::spawn(move || forward_chunks(stderr, &sender, id, ProcEvent::Stderr));

    let sender = instance.events.clone();
    let waited = child.clone();
    std::thread::spawn(move || {
        let code = loop {
            match lock(&waited).try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) => {}
                Err(err) => {
                    log::warn!("waiting for a plugin process: {err}");
                    break None;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        for reader in [stdout_reader, stderr_reader] {
            if reader.join().is_err() {
                log::warn!("a plugin process reader panicked");
            }
        }
        if sender.send(Event::Proc { id, event: ProcEvent::Exit(code) }).is_err() {
            log::debug!("plugin instance gone before its process {id} exited");
        }
    });

    Ok(Started { child, stdin, pid })
}

/// The environment a login shell of the user has (`$SHELL -l`), or `None`
/// where there is no such thing to ask (Windows: the daemon already runs
/// with the user's environment) or it could not be read.
///
/// Why: on a remote host the daemon is started by a relay that ssh runs as
/// a plain command, not a login session, so its PATH lacks what the
/// user's profile adds (`~/.local/bin`, where `claude` is installed).
fn login_environment() -> Option<HashMap<String, String>> {
    if cfg!(windows) {
        return None;
    }
    match read_login_environment() {
        Ok(env) => Some(env),
        Err(err) => {
            log::warn!("plugin processes get the daemon's environment: the login shell's could not be read: {err:#}");
            None
        }
    }
}

/// Printed before the environment, so whatever the profile prints first is skipped.
const ENV_MARKER: &str = "__SOM_PLUGIN_ENV__";

fn read_login_environment() -> anyhow::Result<HashMap<String, String>> {
    let shell = std::env::var("SHELL").ok().filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let script = format!("printf '\n{ENV_MARKER}\n'; env -0 2>/dev/null || env");
    let mut child = Command::new(&shell)
        .args(["-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill()?;
            anyhow::bail!("{shell} -l did not finish in 10 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))??;
    parse_environment(&output).ok_or_else(|| anyhow::anyhow!("{shell} -l printed no environment"))
}

/// The variables after the last marker line: NUL-separated (`env -0`), or
/// one per line where `env -0` is missing.
fn parse_environment(output: &[u8]) -> Option<HashMap<String, String>> {
    let marker = format!("\n{ENV_MARKER}\n");
    let text = String::from_utf8_lossy(output);
    let start = text.rfind(&marker)? + marker.len();
    let body = &text[start..];
    let separator = if body.contains('\0') { '\0' } else { '\n' };
    let env: HashMap<String, String> = body
        .split(separator)
        .filter_map(|entry| entry.split_once('='))
        .filter(|(name, _)| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    env.contains_key("PATH").then_some(env)
}

fn forward_chunks(mut stream: impl Read, sender: &mpsc::Sender<Event>, id: u64, wrap: fn(Vec<u8>) -> ProcEvent) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if sender.send(Event::Proc { id, event: wrap(buffer[..read].to_vec()) }).is_err() {
                    return;
                }
            }
        }
    }
}

/// The Lua side of the API: `plugin.on`, `require` over the plugin's own
/// files, text-only `load`, `print` to the log, and the `som.*` calls that
/// wait for Som's answer.
const PRELUDE: &str = r#"
local handlers = {}
plugin = {}
function plugin.on(name, fn)
  if type(fn) ~= "function" then error("plugin.on: a function is needed for " .. tostring(name), 2) end
  handlers[name] = fn
end
function __plugin_handler(name) return handlers[name] end

local raw_load = __load
__load = nil
function load(chunk, name, mode, env)
  return raw_load(chunk, name, "t", env)
end

local sources = __sources
__sources = nil
local loaded = {}
function require(name)
  if loaded[name] ~= nil then return loaded[name] end
  local base = string.gsub(name, "%.", "/")
  local path = base .. ".lua"
  local text = sources[path]
  if text == nil then
    path = base .. "/init.lua"
    text = sources[path]
  end
  if text == nil then error("module " .. name .. " is not among the plugin's files", 2) end
  local chunk, err = raw_load(text, "@" .. path, "t")
  if not chunk then error(err, 0) end
  local result = chunk(name)
  if result == nil then result = true end
  loaded[name] = result
  return result
end

function print(...) log.info(...) end

local send = __som_send
__som_send = nil
local function call(request)
  local id, err = send(request)
  if not id then return nil, err end
  local ok, value = coroutine.yield("__som_wait", id)
  if ok then return value end
  return nil, value
end
local has = __som_has
__som_has = nil
som = {
  api = __som_api,
  has = function(name) return has[name] == true end,
  open_terminal = function(t) return call({ OpenTerminal = { cmd = t.cmd, cwd = t.cwd } }) end,
  notify = function(t) return call({ Notify = { title = t.title, body = t.body } }) end,
  preview = function(path) return call({ Preview = { path = path } }) end,
  clipboard_write = function(text) return call({ ClipboardWrite = { text = text } }) end,
  confirm = function(t) return call({ Confirm = { text = t.text, buttons = t.buttons } }) end,
  open_url = function(url) return call({ OpenUrl = { url = url } }) end,
  open_plugin = function(t) return call({ OpenPlugin = { name = t.name, config = t.config or {} } }) end,
}
__som_api = nil
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_after_the_marker_is_read_and_profile_noise_skipped() {
        let output = format!("Welcome to deb!\nLast login: today\n\n{ENV_MARKER}\nPATH=/home/u/.local/bin:/usr/bin\0HOME=/home/u\0MULTI=a\nb\0");
        let env = parse_environment(output.as_bytes()).expect("an environment");
        assert_eq!(env["PATH"], "/home/u/.local/bin:/usr/bin");
        assert_eq!(env["MULTI"], "a\nb");
        assert_eq!(env["HOME"], "/home/u");
    }

    #[test]
    fn without_env_0_one_variable_per_line() {
        let output = format!("\n{ENV_MARKER}\nPATH=/usr/bin\nHOME=/home/u\n");
        let env = parse_environment(output.as_bytes()).expect("an environment");
        assert_eq!(env["HOME"], "/home/u");
    }

    #[test]
    fn no_marker_or_no_path_is_no_environment() {
        assert!(parse_environment(b"just a motd\n").is_none());
        assert!(parse_environment(format!("\n{ENV_MARKER}\nHOME=/x\0").as_bytes()).is_none());
    }
}
