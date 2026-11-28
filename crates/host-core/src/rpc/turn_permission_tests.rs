//! The per-turn permission ceiling through the real RPC and permission gate.
use super::*;
use std::fs;

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<Mutex<AppState>>,
    root: PathBuf,
    session_id: String,
}

impl Fixture {
    /// A local Agent session whose stored mode is `auto`.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        let mut app = AppState::open(&dir.path().join("data")).unwrap();
        app.handshook = true;
        let state = Arc::new(Mutex::new(app));
        let session = handle_request(
            state.clone(),
            "session.create",
            json!({ "projectPath": root, "mode": "agent" }),
            mpsc::unbounded_channel().0,
        )
        .await
        .unwrap();
        let fixture = Self {
            _dir: dir,
            state,
            root: PathBuf::from(session["session"]["projectPath"].as_str().unwrap()),
            session_id: session["session"]["id"].as_str().unwrap().to_owned(),
        };
        sessions::configure_session_with_thinking(
            &fixture.state.lock().await.db,
            &fixture.session_id,
            "agent",
            None,
            None,
            None,
            Some("auto"),
        )
        .unwrap();
        fixture
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, JsonRpcError> {
        handle_request(
            self.state.clone(),
            method,
            params,
            mpsc::unbounded_channel().0,
        )
        .await
    }

    async fn begin(&self, ceiling: Option<&str>) -> String {
        let mut params = json!({ "sessionId": self.session_id });
        if let Some(ceiling) = ceiling {
            params["permissionCeiling"] = json!(ceiling);
        }
        let begun = self.rpc("session.beginTurn", params).await.unwrap();
        assert_eq!(begun["permissionCeiling"], json!(ceiling));
        begun["turnId"].as_str().unwrap().to_owned()
    }

    async fn end(&self, turn_id: &str) {
        self.rpc(
            "session.endTurn",
            json!({ "turnId": turn_id, "createNotification": false }),
        )
        .await
        .unwrap();
    }

    /// Start a Write and return the approval request it raised, if any.
    async fn write(
        &self,
        turn_id: &str,
        call: &str,
        scope: Option<&str>,
    ) -> (
        tokio::task::JoinHandle<Result<Value, JsonRpcError>>,
        Option<String>,
    ) {
        let mut params = json!({
            "sessionId": self.session_id,
            "turnId": turn_id,
            "toolCallId": call,
            "toolName": "Write",
            "args": { "path": format!("{call}.txt"), "content": "fixture" },
            "mode": "agent",
        });
        if let Some(scope) = scope {
            params["permissionScope"] = json!(scope);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let state = self.state.clone();
        let pending =
            tokio::spawn(async move { handle_request(state, "tools.execute", params, tx).await });
        let request = match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(text)) => {
                let notification: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(notification["method"], "permissions.request");
                Some(
                    notification["params"]["requestId"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            }
            _ => None,
        };
        (pending, request)
    }

    /// Permission mode of a new session inheriting from this one.
    async fn child_mode(&self) -> Value {
        let created = self
            .rpc(
                "session.create",
                json!({ "projectPath": self.root, "inheritPermissionFromSessionId": self.session_id }),
            )
            .await
            .unwrap();
        created["session"]["permissionMode"].clone()
    }

    async fn resolve(&self, request_id: &str, decision: &str) {
        self.rpc(
            "permissions.resolve",
            json!({ "requestId": request_id, "decision": decision }),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn a_phone_turn_asks_desktop_in_an_auto_session_and_leaves_the_session_unchanged() {
    let fixture = Fixture::new().await;
    let phone = fixture.begin(Some("ask")).await;

    // Denied: the gated write has no side effect.
    let (pending, request) = fixture.write(&phone, "denied", None).await;
    fixture
        .resolve(
            request.as_deref().expect("the ceiling requires approval"),
            "deny",
        )
        .await;
    let denied = pending.await.unwrap().unwrap();
    assert_eq!(denied["errorCode"], "TOOL_DENIED");
    assert!(!fixture.root.join("denied.txt").exists());

    // A delegate's own broader scope cannot lift the turn's ceiling.
    let (pending, request) = fixture.write(&phone, "delegated", Some("auto")).await;
    fixture
        .resolve(
            request.as_deref().expect("a delegate still asks"),
            "allow-once",
        )
        .await;
    assert_eq!(pending.await.unwrap().unwrap()["ok"], true);
    assert!(fixture.root.join("delegated.txt").exists());

    // Delegated work that outlives the phone turn keeps answering to it.
    fixture.end(&phone).await;
    let desktop = fixture.begin(None).await;
    let (pending, request) = fixture.write(&phone, "late-delegate", None).await;
    fixture
        .resolve(
            request.as_deref().expect("the ended phone turn still asks"),
            "deny",
        )
        .await;
    assert_eq!(pending.await.unwrap().unwrap()["errorCode"], "TOOL_DENIED");

    // The next Desktop turn runs under the unchanged session setting.
    let (pending, request) = fixture.write(&desktop, "desktop", None).await;
    assert!(request.is_none(), "an auto Desktop turn must not prompt");
    assert_eq!(pending.await.unwrap().unwrap()["ok"], true);
    let stored =
        sessions::session_permission_mode(&fixture.state.lock().await.db, &fixture.session_id)
            .unwrap();
    assert_eq!(stored.as_deref(), Some("auto"));
    let evaluated = fixture
        .rpc(
            "permissions.evaluate",
            json!({ "sessionId": fixture.session_id, "turnId": phone, "toolName": "Write" }),
        )
        .await
        .unwrap();
    assert!(evaluated["decision"].is_null(), "{evaluated}");
}

#[tokio::test]
async fn an_unusable_ceiling_starts_no_turn() {
    let fixture = Fixture::new().await;
    let error = fixture
        .rpc(
            "session.beginTurn",
            json!({ "sessionId": fixture.session_id, "permissionCeiling": "inherit" }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.data.unwrap()["errorCode"], "INVALID_PARAMS");
    // No running turn was left behind: the session can still begin one.
    fixture.begin(Some("ask")).await;
}

#[tokio::test]
async fn a_session_created_during_a_phone_turn_starts_no_broader() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.child_mode().await, "auto");
    let phone = fixture.begin(Some("ask")).await;
    assert_eq!(fixture.child_mode().await, "ask");
    fixture.end(&phone).await;
    assert_eq!(fixture.child_mode().await, "auto");
}

#[tokio::test]
async fn a_phone_turn_cannot_arrange_an_automatic_desktop_continuation() {
    let fixture = Fixture::new().await;
    fixture
        .state
        .lock()
        .await
        .db
        .set_setting("app", &json!({ "backgroundJobContinuationEnabled": true }))
        .unwrap();
    let phone = fixture.begin(Some("ask")).await;
    let error = fixture
        .rpc(
            "tools.execute",
            json!({
                "sessionId": fixture.session_id,
                "turnId": phone,
                "toolCallId": "background",
                "toolName": "Bash",
                "args": { "command": "true", "runInBackground": true, "notifyOnExit": true },
                "runInBackground": true,
                "mode": "agent",
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.data.unwrap()["errorCode"], "PERMISSION_DENIED");
}
