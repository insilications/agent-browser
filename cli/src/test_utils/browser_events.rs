//! Deterministic CDP peer for startup events, shared by daemon and MCP tests.

use super::EnvGuard;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

pub const IFRAME_SESSION: &str = "iframe-session";
pub const PAGE_SESSION: &str = "page-session";
pub const TEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub enum RecoveryFailure {
    #[default]
    None,
    Preparation,
    Stall,
}

pub struct Scenario {
    pub dialog: &'static str,
    pub auto_dialog: bool,
    pub attachment_event: bool,
    pub failure: RecoveryFailure,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            dialog: "alert",
            auto_dialog: true,
            attachment_event: false,
            failure: RecoveryFailure::None,
        }
    }
}

pub struct BrowserEventServer {
    pub url: String,
    commands: Arc<Mutex<Vec<Value>>>,
    received: Arc<Notify>,
    events: mpsc::UnboundedSender<(Value, oneshot::Sender<()>)>,
    task: JoinHandle<()>,
}

impl BrowserEventServer {
    pub async fn start(scenario: Scenario) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let commands = Arc::new(Mutex::new(Vec::new()));
        let log = commands.clone();
        let received = Arc::new(Notify::new());
        let notification = received.clone();
        let (events, mut event_rx) = mpsc::unbounded_channel::<(Value, oneshot::Sender<()>)>();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut fetch_sent = false;
            let mut dialog_sent = false;
            let mut pending = HashSet::<String>::new();
            let mut held_recovery = None;
            loop {
                let command: Value = tokio::select! {
                    Some((event, sent)) = event_rx.recv() => {
                        if ws.send(Message::Text(event.to_string())).await.is_err() { break; }
                        let _ = sent.send(());
                        continue;
                    }
                    message = ws.next() => match message {
                        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).unwrap(),
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => continue,
                    }
                };
                log.lock().unwrap().push(command.clone());
                notification.notify_one();
                let method = command["method"].as_str().unwrap();
                let sid = command["sessionId"].as_str().unwrap_or("");
                let mut result = json!({});
                let mut emitted = Vec::new();
                match method {
                    "Target.getTargets" => {
                        result = json!({"targetInfos": [
                            {"targetId":"page", "type":"page", "url":"https://page.test/", "title":"Page"},
                            {"targetId":"iframe", "type":"iframe", "url":"https://frame.test/", "title":"Frame"}
                        ]})
                    }
                    "Target.attachToTarget" => {
                        let iframe = command["params"]["targetId"] == "iframe";
                        if iframe && matches!(scenario.failure, RecoveryFailure::Stall) {
                            continue;
                        }
                        result = json!({"sessionId": if iframe { IFRAME_SESSION } else { PAGE_SESSION }});
                        if iframe && scenario.attachment_event {
                            emitted.push(json!({"method":"Target.attachedToTarget", "params":{
                                "sessionId":IFRAME_SESSION, "waitingForDebugger":true,
                                "targetInfo":{"targetId":"iframe", "type":"iframe", "url":"https://frame.test/", "title":"Frame"}
                            }}));
                        }
                    }
                    "Runtime.evaluate" => result = json!({"result":{"type":"number", "value":1}}),
                    "Page.addScriptToEvaluateOnNewDocument" => {
                        result = json!({"identifier":"script"})
                    }
                    "Page.getFrameTree" => {
                        let mut tree = json!({"frame":{
                            "id": if sid == IFRAME_SESSION {"iframe"} else {"page"},
                            "loaderId":"loader", "url":"https://page.test/",
                            "securityOrigin":"https://page.test", "mimeType":"text/html"
                        }});
                        if sid == PAGE_SESSION {
                            tree["childFrames"] = json!([{"frame":{
                                "id":"iframe", "parentId":"page", "loaderId":"child-loader",
                                "url":"https://frame.test/", "securityOrigin":"https://frame.test", "mimeType":"text/html"
                            }}]);
                        }
                        result = json!({"frameTree":tree});
                        // runIfWaitingForDebugger is fire-and-forget. Gate the
                        // awaited topology refresh instead, so subscription
                        // setup cannot finish until recovery events are handled.
                        if dialog_sent && !pending.is_empty() {
                            held_recovery = Some(json!({"id":command["id"], "result":result}));
                        }
                    }
                    "Fetch.enable" if sid == IFRAME_SESSION && !fetch_sent => {
                        fetch_sent = true;
                        for (id, url) in [
                            ("recovery-request", "https://frame.test/data"),
                            ("recovery-blocked", "https://blocked.test/data"),
                        ] {
                            pending.insert(id.into());
                            emitted.push(paused_request(id, url));
                        }
                        if command["params"]["handleAuthRequests"] == true {
                            pending.insert("recovery-auth".into());
                            emitted.push(json!({"method":"Fetch.authRequired", "sessionId":IFRAME_SESSION, "params":{
                                "requestId":"recovery-auth", "request":{"url":"https://frame.test/data"},
                                "authChallenge":{"source":"Proxy", "origin":"http://proxy.test", "scheme":"basic", "realm":"test"}
                            }}));
                        }
                    }
                    "Runtime.runIfWaitingForDebugger" if sid == IFRAME_SESSION && !dialog_sent => {
                        dialog_sent = true;
                        emitted.push(dialog_event(IFRAME_SESSION, scenario.dialog));
                        if scenario.auto_dialog
                            && matches!(scenario.dialog, "alert" | "beforeunload")
                        {
                            pending.insert("dialog".into());
                        }
                    }
                    "Fetch.continueRequest" | "Fetch.failRequest" | "Fetch.continueWithAuth" => {
                        if sid == IFRAME_SESSION {
                            if let Some(id) = command["params"]["requestId"].as_str() {
                                pending.remove(id);
                            }
                        }
                    }
                    "Page.handleJavaScriptDialog" if sid == IFRAME_SESSION => {
                        pending.remove("dialog");
                    }
                    _ => {}
                }
                for event in emitted {
                    if ws.send(Message::Text(event.to_string())).await.is_err() {
                        return;
                    }
                }
                let response = if method == "Page.enable"
                    && sid == IFRAME_SESSION
                    && matches!(scenario.failure, RecoveryFailure::Preparation)
                {
                    json!({"id":command["id"], "error":{"code":-32000, "message":"injected iframe preparation failure"}})
                } else {
                    json!({"id":command["id"], "result":result})
                };
                if held_recovery
                    .as_ref()
                    .is_none_or(|held| held["id"] != command["id"])
                    && ws.send(Message::Text(response.to_string())).await.is_err()
                {
                    break;
                }
                if pending.is_empty() {
                    if let Some(response) = held_recovery.take() {
                        if ws.send(Message::Text(response.to_string())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            url,
            commands,
            received,
            events,
            task,
        }
    }

    pub fn commands(&self) -> Vec<Value> {
        self.commands.lock().unwrap().clone()
    }

    pub async fn wait_for(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let notified = self.received.notified();
                if let Some(command) = self.commands().into_iter().find(&predicate) {
                    return command;
                }
                notified.await;
            }
        })
        .await
        .expect("expected CDP command was not received")
    }

    pub async fn emit(&self, event: Value) {
        let (tx, rx) = oneshot::channel();
        self.events.send((event, tx)).unwrap();
        tokio::time::timeout(TEST_TIMEOUT, rx)
            .await
            .unwrap()
            .unwrap();
    }

    pub async fn assert_late_events_handled(&self, auto_dialog: bool) {
        self.emit(paused_request("late-request", "https://frame.test/late"))
            .await;
        self.wait_for(|cmd| {
            cmd["method"] == "Fetch.continueRequest" && cmd["params"]["requestId"] == "late-request"
        })
        .await;
        if auto_dialog {
            // Same receiver, different session: this also fences any earlier
            // confirm/prompt event without requiring a response to that event.
            self.emit(dialog_event(PAGE_SESSION, "alert")).await;
            self.wait_for(|cmd| {
                cmd["method"] == "Page.handleJavaScriptDialog" && cmd["sessionId"] == PAGE_SESSION
            })
            .await;
        }
    }

    pub fn assert_recovery_requests(&self, proxy_auth: bool, allowed_domains: bool) {
        let commands = self.commands();
        for (id, method) in [
            ("recovery-request", "Fetch.continueRequest"),
            (
                "recovery-blocked",
                if allowed_domains {
                    "Fetch.failRequest"
                } else {
                    "Fetch.continueRequest"
                },
            ),
        ] {
            let responses: Vec<_> = commands
                .iter()
                .filter(|cmd| cmd["params"]["requestId"] == id)
                .collect();
            assert_eq!(responses.len(), 1, "{id} must be handled exactly once");
            assert_eq!(responses[0]["method"], method);
            assert_eq!(responses[0]["sessionId"], IFRAME_SESSION);
        }
        let auth: Vec<_> = commands
            .iter()
            .filter(|cmd| cmd["method"] == "Fetch.continueWithAuth")
            .collect();
        assert_eq!(auth.len(), usize::from(proxy_auth));
        if proxy_auth {
            assert_eq!(auth[0]["sessionId"], IFRAME_SESSION);
            assert_eq!(auth[0]["params"]["requestId"], "recovery-auth");
            assert_eq!(
                auth[0]["params"]["authChallengeResponse"],
                json!({"response":"ProvideCredentials", "username":"fixture-user", "password":"fixture-password"})
            );
        }
    }

    pub async fn finish(self) {
        tokio::time::timeout(TEST_TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

fn paused_request(id: &str, url: &str) -> Value {
    json!({"method":"Fetch.requestPaused", "sessionId":IFRAME_SESSION, "params":{
        "requestId":id, "request":{"url":url, "method":"GET", "headers":{}}, "resourceType":"XHR", "frameId":"iframe"
    }})
}

pub fn dialog_event(session: &str, kind: &str) -> Value {
    json!({"method":"Page.javascriptDialogOpening", "sessionId":session, "params":{
        "url":"https://frame.test/", "type":kind, "message":"startup event", "hasBrowserHandler":true
    }})
}

/// Isolate every environment setting used by these launch fixtures, including
/// on-disk tab binding and restore state. The caller retains the temp directory.
pub fn isolated_env(directory: &std::path::Path) -> EnvGuard<'static> {
    let vars = [
        "AGENT_BROWSER_CDP",
        "AGENT_BROWSER_AUTO_CONNECT",
        "AGENT_BROWSER_PROVIDER",
        "AGENT_BROWSER_PROXY",
        "AGENT_BROWSER_PROXY_USERNAME",
        "AGENT_BROWSER_PROXY_PASSWORD",
        "AGENT_BROWSER_ALLOWED_DOMAINS",
        "AGENT_BROWSER_NO_AUTO_DIALOG",
        "AGENT_BROWSER_SESSION_NAME",
        "AGENT_BROWSER_SESSION",
        "AGENT_BROWSER_SOCKET_DIR",
        "AGENT_BROWSER_NAMESPACE",
        "AGENT_BROWSER_PIN_TAB",
        "AGENT_BROWSER_ENGINE",
        "AGENT_BROWSER_PLUGINS",
        "AGENT_BROWSER_CONFIG",
        "AGENT_BROWSER_PROFILE",
        "AGENT_BROWSER_ARGS",
        "AGENT_BROWSER_EXTENSIONS",
        "AGENT_BROWSER_STATE",
        "AGENT_BROWSER_RESTORE_SAVE",
        "AGENT_BROWSER_RESTORE_CHECK_URL",
        "AGENT_BROWSER_RESTORE_CHECK_TEXT",
        "AGENT_BROWSER_RESTORE_CHECK_FN",
        "AGENT_BROWSER_INIT_SCRIPTS",
        "AGENT_BROWSER_ENABLE",
        "AGENT_BROWSER_ACTION_POLICY",
        "AGENT_BROWSER_CONFIRM_ACTIONS",
        "AGENT_BROWSER_CA_CERT",
        "BROWSER_USE_API_KEY",
    ];
    let guard = EnvGuard::new(&vars);
    for name in vars {
        guard.remove(name);
    }
    guard.set("AGENT_BROWSER_SOCKET_DIR", directory.to_str().unwrap());
    guard
}

#[cfg(unix)]
pub struct MockProvider {
    directory: tempfile::TempDir,
    pub config: crate::plugins::PluginConfig,
}

#[cfg(unix)]
impl MockProvider {
    pub fn new(url: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("provider.sh");
        std::fs::write(
            &script,
            r#"request=$(cat)
printf '%s\n' "$request" >> "$1"
case "$request" in
  *'"type":"browser.close"'*) cat "$3" ;;
  *) cat "$2" ;;
esac
"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("launch.json"),
            json!({
                "protocol":"agent-browser.plugin.v1", "success":true,
                "browser":{"cdpUrl":url, "cleanup":{"sessionId":"startup-fixture"}}
            })
            .to_string(),
        )
        .unwrap();
        let config = crate::plugins::PluginConfig {
            name: "startup-fixture".into(),
            command: "/bin/sh".into(),
            args: [
                script,
                directory.path().join("requests.jsonl"),
                directory.path().join("launch.json"),
                directory.path().join("close.json"),
            ]
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
            capabilities: vec![crate::plugins::CAPABILITY_BROWSER_PROVIDER.into()],
            ..Default::default()
        };
        let provider = Self { directory, config };
        provider.set_cleanup_success(true);
        provider
    }

    pub fn set_cleanup_success(&self, success: bool) {
        std::fs::write(self.directory.path().join("close.json"), json!({
            "protocol":"agent-browser.plugin.v1", "success":success, "data":{}, "error":"injected cleanup failure"
        }).to_string()).unwrap();
    }

    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
