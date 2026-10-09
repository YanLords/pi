use agon_bridge::protocol::{RawRequest, Response, read_frame, write_event, write_response};
use agon_bridge::service::{Service, ServiceError};
use std::io::{stdin, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("agon-bridge {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "agon-bridge: persistent Agon core daemon sidecar\nUsage: agon-bridge [--root <path>]"
        );
        return Ok(());
    }

    let mut root = std::env::current_dir()?;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--root" && i + 1 < args.len() {
            root = PathBuf::from(&args[i + 1]);
            i += 2;
        } else {
            i += 1;
        }
    }

    let service = match Service::new(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "agon-bridge: failed to initialize service in {}: {e}",
                root.display()
            );
            std::process::exit(1);
        }
    };

    let service = Arc::new(Mutex::new(service));
    let stdout_lock = Arc::new(Mutex::new(stdout()));

    let mut stdin_lock = stdin();

    loop {
        let frame = match read_frame(&mut stdin_lock) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break, // EOF propre
            Err(e) => {
                eprintln!("agon-bridge protocol error: {e}");
                std::process::exit(1);
            }
        };

        let req: RawRequest = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agon-bridge invalid json request: {e}");
                std::process::exit(1);
            }
        };

        let id = req.id;
        let op = req.op.as_str();

        let resp = match op {
            "hello" => {
                let mut svc = service.lock().await;
                match svc.hello() {
                    Ok(val) => Response::ok(id, val),
                    Err(ServiceError::Tampering(v)) => {
                        Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                    }
                    Err(e) => Response::err(id, "service_error", e.to_string()),
                }
            }
            "status" => {
                let mut svc = service.lock().await;
                match svc.status() {
                    Ok(val) => Response::ok(id, val),
                    Err(e) => Response::err(id, "service_error", e.to_string()),
                }
            }
            "authorize" => {
                let action = req
                    .params
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let detail = req
                    .params
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut svc = service.lock().await;
                match svc.authorize(action, detail) {
                    Ok(val) => Response::ok(id, val),
                    Err(ServiceError::Tampering(v)) => {
                        Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                    }
                    Err(ServiceError::UnknownAction(a)) => {
                        Response::err(id, "invalid_action", format!("unknown action `{a}`"))
                    }
                    Err(e) => Response::err(id, "service_error", e.to_string()),
                }
            }
            "verify" => {
                let check_id = req.params.get("check_id").and_then(|v| v.as_str());
                let mut svc = service.lock().await;
                match svc.verify(check_id) {
                    Ok(val) => Response::ok(id, val),
                    Err(ServiceError::Tampering(v)) => {
                        Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                    }
                    Err(ServiceError::UnknownCheck(c)) => {
                        Response::err(id, "unknown_check", format!("unknown check `{c}`"))
                    }
                    Err(e) => Response::err(id, "service_error", e.to_string()),
                }
            }
            "repair" => {
                let check_id = req
                    .params
                    .get("check_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let out_for_event = Arc::clone(&stdout_lock);
                let on_event = move |event_name: &str, data: serde_json::Value| {
                    let out = Arc::clone(&out_for_event);
                    let event = event_name.to_string();
                    tokio::spawn(async move {
                        let mut w = out.lock().await;
                        let _ = write_event(&mut *w, &event, data);
                    });
                };
                let mut svc = service.lock().await;
                match svc.repair(&check_id, on_event).await {
                    Ok(val) => Response::ok(id, val),
                    Err(ServiceError::Tampering(v)) => {
                        Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                    }
                    Err(ServiceError::UnknownCheck(c)) => {
                        Response::err(id, "unknown_check", format!("unknown check `{c}`"))
                    }
                    Err(e) => Response::err(id, "service_error", e.to_string()),
                }
            }
            "shutdown" => {
                let mut svc = service.lock().await;
                let val = svc
                    .shutdown()
                    .unwrap_or_else(|_| serde_json::json!({"status": "shutdown"}));
                let mut out = stdout_lock.lock().await;
                let _ = write_response(&mut *out, Response::ok(id, val));
                break;
            }
            other => Response::err(id, "unknown_op", format!("unsupported operation `{other}`")),
        };

        let mut out = stdout_lock.lock().await;
        if let Err(e) = write_response(&mut *out, resp) {
            eprintln!("agon-bridge write response error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
