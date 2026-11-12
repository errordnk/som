//! The plugin host end to end, without a daemon: a Lua plugin, a fake Som
//! connection (a sink into a channel), events in, protocol messages out.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;
use srp_client::plugin::{SomCall, SomReply, ViewEvent, ViewModel, ViewOp};
use srp_client::protocol::SrvResponse;

use super::{PluginHost, Sink, StartRequest};

const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    host: PluginHost,
    id: String,
    messages: mpsc::Receiver<SrvResponse>,
    /// What Som would have drawn: every op applied in order.
    view: ViewModel,
}

fn sink() -> (Sink, mpsc::Receiver<SrvResponse>) {
    let (sender, receiver) = mpsc::channel();
    let sink: Sink = std::sync::Arc::new(move |message: &SrvResponse| {
        sender.send(message.clone()).map_err(|_| anyhow::anyhow!("closed"))
    });
    (sink, receiver)
}

fn start_with(main: &str, manifest: serde_json::Value, state: Option<serde_json::Value>) -> Fixture {
    let host = PluginHost::new();
    let id = uuid::Uuid::new_v4().to_string();
    let (sink, messages) = sink();
    let request = StartRequest {
        manifest,
        sources: vec![("main.lua".into(), main.into())],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        config: json!({ "greeting": "hi" }),
        state,
    };
    host.start(&id, request, 1, sink).unwrap();
    Fixture { host, id, messages, view: ViewModel::default() }
}

fn start(main: &str) -> Fixture {
    start_with(main, json!({ "name": "test", "version": "1", "permissions": ["notify"] }), None)
}

impl Fixture {
    /// Reads messages (applying view ops) until `done` says so.
    fn until(&mut self, what: &str, mut done: impl FnMut(&SrvResponse, &ViewModel) -> bool) -> SrvResponse {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self.messages.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let SrvResponse::PluginView { op, .. } = &message {
                apply(&mut self.view, op);
            }
            if done(&message, &self.view) {
                return message;
            }
        }
    }

    fn until_view(&mut self, what: &str, done: impl Fn(&ViewModel) -> bool) {
        self.until(what, |_, view| done(view));
    }

    fn event(&self, event: ViewEvent) {
        self.host.event(&self.id, event).unwrap();
    }

    fn texts(&self) -> Vec<String> {
        self.view.blocks.iter().filter_map(|block| block.props["text"].as_str().map(str::to_owned)).collect()
    }
}

fn apply(view: &mut ViewModel, op: &ViewOp) {
    let mut model = super::model::Model { view: view.clone(), ..Default::default() };
    model.apply(op).unwrap();
    *view = model.view;
}

#[test]
fn start_submit_and_a_snapshot_for_a_later_attach() {
    let mut fixture = start(
        r#"
        plugin.on("start", function(ctx)
          ui.header{ title = "Echo", status = "idle" }
          ui.input{ enabled = true }
          ui.append{ kind = "text", props = { text = ctx.config.greeting } }
        end)
        plugin.on("submit", function(text)
          ui.batch(function()
            local id = ui.append{ kind = "markdown", props = { text = "" } }
            ui.update(id, { text = "you said " .. text })
          end)
        end)
        "#,
    );
    fixture.until_view("start", |view| view.blocks.len() == 1);
    assert_eq!(fixture.view.header.title.as_deref(), Some("Echo"));
    assert!(fixture.view.input.enabled);

    fixture.event(ViewEvent::Submit { text: "привет".into() });
    let batch = fixture.until("the batch", |message, _| matches!(message, SrvResponse::PluginView { op: ViewOp::Batch(_), .. }));
    assert!(matches!(batch, SrvResponse::PluginView { op: ViewOp::Batch(ops), .. } if ops.len() == 2));
    assert_eq!(fixture.texts(), ["hi", "you said привет"]);

    let (sink, later) = sink();
    fixture.host.attach(&fixture.id, 2, sink).unwrap();
    match later.recv_timeout(WAIT).unwrap() {
        SrvResponse::PluginView { op: ViewOp::Snapshot(view), .. } => assert_eq!(view, fixture.view),
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[test]
fn the_sandbox_has_no_files_no_os_and_only_the_plugins_modules() {
    let mut fixture = start(
        r#"
        local function report(name, value) ui.append{ kind = "text", props = { text = name .. "=" .. tostring(value) } } end
        plugin.on("start", function()
          report("io", io); report("os", os); report("package", package)
          report("dofile", dofile); report("loadfile", loadfile)
          report("require_outside", (pcall(require, "socket")))
          report("bytecode", (load(string.dump(function() end))))
          report("spawn_unlisted", (pcall(proc.spawn, { program = "sh" })))
          report("done", true)
        end)
        "#,
    );
    fixture.until_view("the report", |view| view.blocks.len() == 9);
    assert_eq!(
        fixture.texts(),
        [
            "io=nil",
            "os=nil",
            "package=nil",
            "dofile=nil",
            "loadfile=nil",
            "require_outside=false",
            "bytecode=nil",
            "spawn_unlisted=false",
            "done=true"
        ]
    );
}

#[test]
fn a_runaway_handler_is_stopped_and_the_instance_goes_on() {
    let mut fixture = start(
        r#"
        plugin.on("submit", function(text)
          if text == "loop" then while true do end end
          ui.append{ kind = "text", props = { text = text } }
        end)
        "#,
    );
    fixture.event(ViewEvent::Submit { text: "loop".into() });
    fixture.event(ViewEvent::Submit { text: "still here".into() });
    fixture.until_view("the next submit", |view| view.blocks.len() == 1);
    assert!(fixture.view.header.error.as_deref().is_some_and(|error| error.contains("longer than")));
    assert_eq!(fixture.texts(), ["still here"]);
}

#[test]
fn som_calls_wait_for_the_answer_and_need_their_permission() {
    let mut fixture = start(
        r#"
        plugin.on("submit", function()
          local value, err = som.notify{ title = "done" }
          ui.append{ kind = "text", props = { text = "notify " .. tostring(value) .. " " .. tostring(err) } }
          local _, denied = som.clipboard_write("x")
          ui.append{ kind = "text", props = { text = "clipboard " .. tostring(denied) } }
        end)
        "#,
    );
    fixture.event(ViewEvent::Submit { text: String::new() });
    let request = match fixture.until("the request", |message, _| matches!(message, SrvResponse::PluginSomRequest { .. })) {
        SrvResponse::PluginSomRequest { request, call, .. } => {
            assert_eq!(call, SomCall::Notify { title: "done".into(), body: None });
            request
        }
        _ => unreachable!(),
    };
    fixture.host.som_reply(&fixture.id, request, SomReply::Ok(json!("shown"))).unwrap();
    fixture.until_view("the answer", |view| view.blocks.len() == 2);
    assert_eq!(fixture.texts(), ["notify shown nil", "clipboard denied"]);
}

#[test]
fn a_waiting_call_gets_detached_when_som_goes_away() {
    let mut fixture = start(
        r#"
        plugin.on("submit", function()
          local _, err = som.notify{ title = "done" }
          ui.append{ kind = "text", props = { text = tostring(err) } }
        end)
        plugin.on("detach", function() ui.header{ status = "detached" } end)
        "#,
    );
    fixture.event(ViewEvent::Submit { text: String::new() });
    fixture.until("the request", |message, _| matches!(message, SrvResponse::PluginSomRequest { .. }));
    fixture.host.detach_connection(1);
    let (sink, later) = sink();
    std::thread::sleep(Duration::from_millis(200));
    fixture.host.attach(&fixture.id, 2, sink).unwrap();
    fixture.messages = later;
    fixture.until_view("the detached answer", |view| view.blocks.len() == 1);
    assert_eq!(fixture.texts(), ["detached"]);
    assert_eq!(fixture.view.header.status.as_deref(), Some("detached"));
}

#[test]
fn a_process_s_lines_and_exit_become_events() {
    let (program, args) = if cfg!(windows) {
        ("cmd", r#"{ "/C", "echo one&& echo two&& exit 3" }"#)
    } else {
        ("sh", r#"{ "-c", "echo one; echo two; exit 3" }"#)
    };
    let main = format!(
        r#"
        plugin.on("start", function()
          proc.spawn{{ program = "{program}", args = {args} }}
        end)
        plugin.on("line", function(p, line) ui.append{{ kind = "text", props = {{ text = line }} }} end)
        plugin.on("exit", function(p, code) ui.append{{ kind = "text", props = {{ text = "exit " .. tostring(code) }} }} end)
        "#
    );
    let mut fixture = start_with(&main, json!({ "name": "test", "version": "1", "spawn": [program] }), None);
    fixture.until_view("the exit", |view| view.blocks.len() == 3);
    let texts: Vec<String> = fixture.texts().iter().map(|text| text.trim().to_owned()).collect();
    assert_eq!(texts, ["one", "two", "exit 3"]);
}

#[test]
fn state_is_sent_to_som_and_given_back_on_a_restart() {
    let mut fixture = start(
        r#"
        plugin.on("start", function(ctx)
          if ctx.state then
            ui.append{ kind = "text", props = { text = "resumed " .. ctx.state.sid } }
          else
            state.save{ sid = "abc" }
          end
        end)
        "#,
    );
    let saved = match fixture.until("the state", |message, _| matches!(message, SrvResponse::PluginState { .. })) {
        SrvResponse::PluginState { state, .. } => state,
        _ => unreachable!(),
    };
    assert_eq!(saved, json!({ "sid": "abc" }));

    let main = r#"
        plugin.on("start", function(ctx) ui.append{ kind = "text", props = { text = "resumed " .. ctx.state.sid } } end)
    "#;
    let mut again = start_with(main, json!({ "name": "test", "version": "1" }), Some(saved));
    again.until_view("the resumed view", |view| view.blocks.len() == 1);
    assert_eq!(again.texts(), ["resumed abc"]);
}

#[test]
fn timers_fire_and_stop_ends_the_instance() {
    let mut fixture = start(
        r#"
        local ticks = 0
        plugin.on("start", function() timer.every(20, "tick") end)
        plugin.on("timer", function(name)
          ticks = ticks + 1
          if ticks == 3 then timer.cancel("tick"); ui.append{ kind = "text", props = { text = name .. " x3" } } end
        end)
        "#,
    );
    fixture.until_view("three ticks", |view| view.blocks.len() == 1);
    assert_eq!(fixture.texts(), ["tick x3"]);

    fixture.host.stop(&fixture.id).unwrap();
    fixture.until("the exit", |message, _| matches!(message, SrvResponse::PluginExited { error: None, .. }));
    let deadline = Instant::now() + WAIT;
    while !fixture.host.is_empty() {
        assert!(Instant::now() < deadline, "the instance stayed in the host");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_plugin_that_fails_to_load_exits_with_the_error() {
    let mut fixture = start("this is not lua");
    let exited = fixture.until("the exit", |message, _| matches!(message, SrvResponse::PluginExited { .. }));
    assert!(matches!(exited, SrvResponse::PluginExited { error: Some(error), .. } if error.contains("main.lua")));
}

#[test]
fn a_plugin_needing_a_newer_api_is_refused() {
    let host = PluginHost::new();
    let (sink, _messages) = sink();
    let request = StartRequest {
        manifest: json!({ "name": "future", "version": "1", "api": 99 }),
        sources: Vec::new(),
        cwd: ".".into(),
        config: json!({}),
        state: None,
    };
    assert!(host.start("x", request, 1, sink).is_err());
    assert!(host.is_empty());
}

/// The repository's own test plugin (`plugins/echo`), as Som would send it.
#[test]
fn the_echo_plugin_starts_and_echoes() {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/echo");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(folder.join("plugin.json")).expect("plugin.json")).expect("json");
    let main = std::fs::read_to_string(folder.join("main.lua")).expect("main.lua");
    let mut fixture = start_with(&main, manifest, None);
    fixture.until_view("the start", |view| view.header.title.as_deref() == Some("Echo") && view.input.enabled);

    fixture.event(ViewEvent::Submit { text: "hi".into() });
    fixture.until_view("the echo", |view| view.blocks.iter().any(|block| block.kind == "markdown"));
    let echoed = fixture.view.blocks.iter().find(|block| block.kind == "markdown").expect("markdown");
    assert_eq!(echoed.props["text"], json!("**#1** you said: hi"));
    assert!(fixture.view.header.error.is_none(), "{:?}", fixture.view.header.error);

    fixture.event(ViewEvent::Submit { text: "/tick".into() });
    fixture.until_view("three ticks", |view| fixture_texts(view).iter().any(|text| text == "tick 3"));
}

fn fixture_texts(view: &ViewModel) -> Vec<String> {
    view.blocks.iter().filter_map(|block| block.props["text"].as_str().map(str::to_owned)).collect()
}
