use agon_bridge::protocol::{OutgoingMessage, RawRequest, Response, read_frame, write_frame};
use serde_json::json;
use std::process::{Command, Stdio};

fn setup_fixture(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agon-bridge-int-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".agon")).unwrap();

    let cfg = r#"
[form]
tools = ["cargo"]
checks = ["verify.ok"]

[permissions]
shell_allow = ["cargo test"]
"#;

    let checks = r#"
[check."verify.ok"]
command = "true"
success = "exit_code == 0"

[check."verify.fail"]
command = "false"
success = "exit_code == 0"

[check."verify.slow"]
command = "sleep 1"
success = "exit_code == 0"
"#;

    std::fs::write(dir.join(".agon/config.toml"), cfg).unwrap();
    std::fs::write(dir.join(".agon/checks.toml"), checks).unwrap();
    dir
}

fn send_req(stdin: &mut std::process::ChildStdin, id: u64, op: &str, params: serde_json::Value) {
    let req = RawRequest {
        id,
        op: op.to_string(),
        params,
    };
    let bytes = serde_json::to_vec(&req).unwrap();
    write_frame(stdin, &bytes).unwrap();
}

fn recv_msg(stdout: &mut std::process::ChildStdout) -> OutgoingMessage {
    let frame = read_frame(stdout).unwrap().expect("frame");
    serde_json::from_slice(&frame).unwrap()
}

fn recv_resp(stdout: &mut std::process::ChildStdout) -> Response {
    loop {
        match recv_msg(stdout) {
            OutgoingMessage::Response(resp) => return resp,
            OutgoingMessage::Event(_) => continue,
        }
    }
}

#[test]
fn test_bridge_binary_cli_and_stdio_lifecycle() {
    let fixture = setup_fixture("cli");
    let bin = env!("CARGO_BIN_EXE_agon-bridge");

    // 1. --version aligned on npm package version
    let version_out = Command::new(bin)
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(version_out.status.success());
    let version_str = String::from_utf8_lossy(&version_out.stdout);
    assert!(
        version_str.contains("agon-bridge 1.1.0"),
        "version was: {version_str}"
    );

    // 2. Start bridge daemon
    let mut child = Command::new(bin)
        .arg("--root")
        .arg(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agon-bridge");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    // Hello
    send_req(&mut stdin, 1, "hello", json!({}));
    let r1 = recv_resp(&mut stdout);
    assert_eq!(r1.id(), 1);
    match r1 {
        Response::Ok { result, .. } => assert_eq!(result["protocol"], 1),
        _ => panic!("expected ok response"),
    }

    // Status
    send_req(&mut stdin, 2, "status", json!({}));
    let r2 = recv_resp(&mut stdout);
    assert_eq!(r2.id(), 2);
    match r2 {
        Response::Ok { result, .. } => {
            assert_eq!(result["active_checks"], json!(["verify.ok"]));
            assert_eq!(result["tampering_violations"], json!([]));
        }
        _ => panic!("expected ok response"),
    }

    // Authorize allow
    send_req(
        &mut stdin,
        3,
        "authorize",
        json!({"action": "shell", "detail": "cargo test"}),
    );
    let r3 = recv_resp(&mut stdout);
    assert_eq!(r3.id(), 3);
    match r3 {
        Response::Ok { result, .. } => assert_eq!(result["verdict"], "allow"),
        _ => panic!("expected ok response"),
    }

    // Authorize deny
    send_req(
        &mut stdin,
        4,
        "authorize",
        json!({"action": "shell", "detail": "git push origin main"}),
    );
    let r4 = recv_resp(&mut stdout);
    assert_eq!(r4.id(), 4);
    match r4 {
        Response::Ok { result, .. } => assert_eq!(result["verdict"], "deny"),
        _ => panic!("expected ok response"),
    }

    // Verify structured response { results: [...] }
    send_req(&mut stdin, 5, "verify", json!({"check_id": "verify.ok"}));
    let r5 = recv_resp(&mut stdout);
    assert_eq!(r5.id(), 5);
    match r5 {
        Response::Ok { result, .. } => {
            assert_eq!(result["results"][0]["check_id"], "verify.ok");
            assert_eq!(result["results"][0]["passed"], true);
            assert_eq!(result["results"][0]["exit_code"], 0);
        }
        _ => panic!("expected ok response"),
    }

    // Shutdown
    send_req(&mut stdin, 6, "shutdown", json!({}));
    let r6 = recv_resp(&mut stdout);
    assert_eq!(r6.id(), 6);
    match r6 {
        Response::Ok { result, .. } => assert_eq!(result["status"], "shutdown"),
        _ => panic!("expected ok response"),
    }

    let status = child.wait().expect("wait child");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&fixture);
}

#[test]
fn test_authorize_responds_during_sleep_verify() {
    let fixture = setup_fixture("concurrency");
    let bin = env!("CARGO_BIN_EXE_agon-bridge");

    let mut child = Command::new(bin)
        .arg("--root")
        .arg(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agon-bridge");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    // 1. Launch slow verify check (sleep 1 second) with id 100
    send_req(
        &mut stdin,
        100,
        "verify",
        json!({"check_id": "verify.slow"}),
    );

    // 2. Immediately launch authorize request with id 101
    send_req(
        &mut stdin,
        101,
        "authorize",
        json!({"action": "read", "detail": "src/main.rs"}),
    );

    // 3. First response received MUST be id 101 (authorize), NOT id 100 (slow verify)
    let first = recv_resp(&mut stdout);
    assert_eq!(
        first.id(),
        101,
        "authorize should complete before sleep 1 verify"
    );
    match first {
        Response::Ok { result, .. } => assert_eq!(result["verdict"], "allow"),
        _ => panic!("expected ok response"),
    }

    // 4. Second response is the slow verify
    let second = recv_resp(&mut stdout);
    assert_eq!(second.id(), 100);
    match second {
        Response::Ok { result, .. } => {
            assert_eq!(result["results"][0]["check_id"], "verify.slow");
            assert_eq!(result["results"][0]["passed"], true);
        }
        _ => panic!("expected ok response"),
    }

    // Clean shutdown
    send_req(&mut stdin, 102, "shutdown", json!({}));
    let _ = recv_resp(&mut stdout);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&fixture);
}

#[test]
fn test_tampering_during_operation_returns_tampering_error() {
    let fixture = setup_fixture("tamper-int");
    let bin = env!("CARGO_BIN_EXE_agon-bridge");

    let mut child = Command::new(bin)
        .arg("--root")
        .arg(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agon-bridge");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    // Initial check is ok
    send_req(
        &mut stdin,
        200,
        "authorize",
        json!({"action": "read", "detail": "test"}),
    );
    let r1 = recv_resp(&mut stdout);
    assert_eq!(r1.id(), 200);

    // Tamper with policy file
    std::fs::write(fixture.join(".agon/checks.toml"), "corrupted policy").unwrap();

    // Next request must return tampering error
    send_req(
        &mut stdin,
        201,
        "authorize",
        json!({"action": "read", "detail": "test"}),
    );
    let r2 = recv_resp(&mut stdout);
    assert_eq!(r2.id(), 201);
    match r2 {
        Response::Err { error, .. } => {
            assert_eq!(error.code, "tampering");
        }
        _ => panic!("expected tampering error response"),
    }

    send_req(&mut stdin, 202, "shutdown", json!({}));
    let _ = recv_resp(&mut stdout);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&fixture);
}
