//! Deterministic service admission tests: the child never emits readiness itself.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    service: VoxTerminalService,
    session: String,
    runtime: Arc<TerminalSessionRuntime>,
    script: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let fixture = Self::with_program("#!/bin/sh\nstty -echo\nprintf 'fixture-started\\n'\nwhile IFS= read -r line; do printf 'ACK:%s\\n' \"$line\"; done\n").await;
        fixture.wait_for("fixture-started", 1).await;
        fixture
    }

    async fn with_program(program: &str) -> Self {
        let script = std::env::temp_dir().join(format!(
            "teamy-input-fixture-{}-{}",
            std::process::id(),
            NEXT_SESSION_EPOCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&script, program).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut service = VoxTerminalService::new();
        service.test_pty_config = Some(PtyConfig {
            shell: script.to_string_lossy().into_owned(),
            ..PtyConfig::test()
        });
        let connected = service
            .connect(super::tests::test_connect_request("readiness-fixture", 1))
            .await
            .unwrap();
        let runtime = service.session(&connected.session_id).unwrap();
        Self {
            service,
            session: connected.session_id,
            runtime,
            script,
        }
    }

    async fn wait_for(&self, text: &str, count: usize) {
        let (id, mut changes) = self.runtime.publication.register(self.runtime.sequence());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .runtime
                    .state
                    .lock()
                    .unwrap()
                    .terminal
                    .snapshot()
                    .visible_text()
                    .matches(text)
                    .count()
                    == count
                {
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("fixture output must arrive via actor notification");
        self.runtime.publication.unregister(id);
    }

    async fn text(&self, text: &str) -> Result<TerminalInputResult, TerminalError> {
        self.service
            .send_text(TerminalTextInput {
                session_id: self.session.clone(),
                text: text.into(),
                client_sequence: 2,
                correlation_id: "same-send-id".into(),
            })
            .await
    }

    fn ready(&self) {
        let token = self.runtime.state.lock().unwrap().readiness_token.clone();
        let marker = format!("\x1b]1337;TeamyShellReady;{token}\x07");
        for bytes in marker.as_bytes().chunks(3) {
            VoxTerminalService::process_pty_event(&self.runtime, PtyEvent::Output(bytes.to_vec()));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime.shutdown();
        let _ = std::fs::remove_file(&self.script);
    }
}

fn starting(error: &TerminalError) {
    assert_eq!(error.code, TerminalErrorCode::InvalidRequest);
    assert!(error.retryable);
    assert!(error.message.contains("input was not forwarded"));
}

#[tokio::test]
async fn immediate_input_is_rejected_then_repeated_accepted_sends_are_distinct() {
    let fixture = Fixture::new().await;
    for _ in 0..3 {
        starting(&fixture.text("rejected\n").await.unwrap_err());
    }
    starting(
        &fixture
            .service
            .cancel(TerminalCancelRequest {
                session_id: fixture.session.clone(),
                client_sequence: 3,
                request_sequence: 2,
                reason: "cancel startup".into(),
                correlation_id: String::new(),
            })
            .await
            .unwrap_err(),
    );
    starting(
        &fixture
            .service
            .send_key(TerminalKeyInput {
                session_id: fixture.session.clone(),
                key_code: 65,
                modifiers: 0,
                pressed: true,
                repeat: false,
                client_sequence: 2,
                correlation_id: String::new(),
            })
            .await
            .unwrap_err(),
    );
    starting(
        &fixture
            .service
            .send_mouse(TerminalMouseInput {
                session_id: fixture.session.clone(),
                x: 0,
                y: 0,
                buttons: 1,
                button: 0,
                pressed: true,
                motion: false,
                wheel_x: 0,
                wheel_y: 0,
                client_sequence: 2,
                correlation_id: String::new(),
            })
            .await
            .unwrap_err(),
    );
    starting(
        &fixture
            .service
            .paste(TerminalPasteRequest {
                session_id: fixture.session.clone(),
                policy: TerminalPastePolicy::GuardMultiline,
                source: TerminalPasteSource::Supplied,
                supplied_text: "rejected\n".into(),
                approved_content_id: String::new(),
                client_sequence: 2,
                correlation_id: String::new(),
            })
            .await
            .unwrap_err(),
    );
    // Neither ordinary output nor an incomplete marker admits input.
    VoxTerminalService::process_pty_event(
        &fixture.runtime,
        PtyEvent::Output(b"PS fake> \x1b]133;A\x07\x1b]133;".to_vec()),
    );
    starting(&fixture.text("rejected\n").await.unwrap_err());
    VoxTerminalService::process_pty_event(&fixture.runtime, PtyEvent::Output(b"B\x07".to_vec()));
    starting(&fixture.text("rejected\n").await.unwrap_err());
    fixture.ready();
    for _ in 0..2 {
        assert_eq!(
            fixture.text("accepted\n").await.unwrap().disposition,
            TerminalInputDisposition::Forwarded
        );
    }
    fixture.wait_for("ACK:accepted", 2).await;
    assert!(
        !fixture
            .runtime
            .state
            .lock()
            .unwrap()
            .terminal
            .snapshot()
            .visible_text()
            .contains("rejected")
    );
}

#[tokio::test]
async fn startup_failure_timeout_and_close_never_admit_late_input() {
    for event in [
        PtyEvent::Error("startup failed".into()),
        PtyEvent::Exited(teamy_terminal_pty::PtyExit {
            code: Some(1),
            success: false,
        }),
    ] {
        let fixture = Fixture::new().await;
        VoxTerminalService::process_pty_event(&fixture.runtime, event);
        fixture.ready();
        let error = fixture.text("rejected\n").await.unwrap_err();
        assert_eq!(error.code, TerminalErrorCode::Disconnected);
        assert!(!error.retryable);
        fixture.runtime.shutdown();
        assert!(fixture.runtime.shutdown_started.load(Ordering::Acquire));
        // Repeated close is idempotent after process failure.
        fixture.runtime.shutdown();
    }
    let fixture = Fixture::new().await;
    fixture.runtime.state.lock().unwrap().input_gate = ShellInputGate::Starting {
        deadline: Instant::now(),
    };
    fixture.ready();
    let error = fixture.text("rejected\n").await.unwrap_err();
    assert_eq!(error.code, TerminalErrorCode::Disconnected);
    assert!(!error.retryable);
    fixture
        .service
        .disconnect(TerminalDisconnectRequest {
            session_id: fixture.session.clone(),
            reason: "abort startup".into(),
            client_sequence: 4,
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.text("rejected\n").await.unwrap_err().code,
        TerminalErrorCode::SessionNotFound
    );
}

#[tokio::test]
async fn readiness_and_input_remain_session_and_connection_scoped() {
    let first = Fixture::new().await;
    let second = Fixture::new().await;
    let first_token = first.runtime.state.lock().unwrap().readiness_token.clone();
    VoxTerminalService::process_pty_event(
        &second.runtime,
        PtyEvent::Output(format!("\x1b]1337;TeamyShellReady;{first_token}\x07").into_bytes()),
    );
    starting(&second.text("rejected\n").await.unwrap_err());
    first.ready();
    first.text("first\n").await.unwrap();
    starting(&second.text("rejected\n").await.unwrap_err());
    let foreign = first.service.for_connection();
    assert_eq!(
        foreign
            .send_text(TerminalTextInput {
                session_id: first.session.clone(),
                text: "foreign\n".into(),
                client_sequence: 2,
                correlation_id: String::new(),
            })
            .await
            .unwrap_err()
            .code,
        TerminalErrorCode::SessionNotFound
    );
    second.ready();
    second.text("second\n").await.unwrap();
    first.wait_for("ACK:first", 1).await;
    second.wait_for("ACK:second", 1).await;
}

#[tokio::test]
async fn disconnect_during_startup_is_terminal_and_cleans_up_once() {
    let fixture = Fixture::new().await;
    fixture
        .service
        .disconnect(TerminalDisconnectRequest {
            session_id: fixture.session.clone(),
            reason: "close before ready".into(),
            client_sequence: 3,
        })
        .await
        .unwrap();
    assert!(fixture.runtime.shutdown_started.load(Ordering::Acquire));
    fixture.ready();
    assert_eq!(
        fixture.text("rejected\n").await.unwrap_err().code,
        TerminalErrorCode::SessionNotFound
    );
    fixture.runtime.shutdown();
}

#[tokio::test]
async fn actual_child_exit_before_prompt_rejects_input_and_allows_cleanup() {
    let fixture = Fixture::with_program("#!/bin/sh\nexit 17\n").await;
    let (id, mut changes) = fixture
        .runtime
        .publication
        .register(fixture.runtime.sequence());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.runtime.closed.load(Ordering::Acquire) {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("actual child exit should be observed by the session actor");
    fixture.runtime.publication.unregister(id);
    let error = fixture.text("rejected\n").await.unwrap_err();
    assert_eq!(error.code, TerminalErrorCode::Disconnected);
    assert!(!error.retryable);
    fixture
        .service
        .disconnect(TerminalDisconnectRequest {
            session_id: fixture.session.clone(),
            reason: "failed startup".into(),
            client_sequence: 3,
        })
        .await
        .unwrap();
    assert!(fixture.runtime.shutdown_started.load(Ordering::Acquire));
}
