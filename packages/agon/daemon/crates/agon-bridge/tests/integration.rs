use agon_bridge::protocol::{RawRequest, Response, read_frame, write_frame};
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
"#;

    std::fs::write(dir.join(".agon/config.toml"), cfg).unwrap();
    std::fs::write(dir.join(".agon/checks.toml"), checks).unwrap();
    dir
}

#[test]
fn test_bridge_binary_cli_and_stdio_lifecycle() {
    let fixture = setup_fixture("cli");

    // 1. --version
    let bin = env!("CARGO_BIN_EXE_agon-bridge");
    let version_out = Command::new(bin)
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(version_out.status.success());
    let version_str = String::from_utf8_lossy(&version_out.stdout);
    assert!(version_str.contains("agon-bridge 0.1.0"));

    // 2. Lancer le process bridge avec stdio pipes
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

    // Helper to send request and read response
    let send_req =
        |stdin: &mut std::process::ChildStdin, id: u64, op: &str, params: serde_json::Value| {
            let req = RawRequest {
                id,
                op: op.to_string(),
                params,
            };
            let bytes = serde_json::to_vec(&req).unwrap();
            write_frame(stdin, &bytes).unwrap();
        };

    let recv_resp = |stdout: &mut std::process::ChildStdout| -> Response {
        let frame = read_frame(stdout).unwrap().expect("frame");
        serde_json::from_slice(&frame).unwrap()
    };

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

    // Authorize
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

    // Verify
    send_req(&mut stdin, 5, "verify", json!({"check_id": "verify.ok"}));
    let r5 = recv_resp(&mut stdout);
    assert_eq!(r5.id(), 5);
    match r5 {
        Response::Ok { result, .. } => {
            assert_eq!(result["check_id"], "verify.ok");
            assert_eq!(result["passed"], true);
            assert_eq!(result["exit_code"], 0);
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

    // Le process doit se terminer proprement (exit code 0)
    let status = child.wait().expect("wait child");
    assert!(status.success());

    let _ = std::fs::remove_dir_all(&fixture);
}
