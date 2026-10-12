use agon_bridge::protocol::{
    EventFrame, OutgoingMessage, ProtocolError, RawRequest, Response, read_frame, write_message,
};
use agon_bridge::service::{Service, ServiceError, format_repair_outcome};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};

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
            eprintln!("agon-bridge init error: {e}");
            std::process::exit(1);
        }
    };

    let service = Arc::new(RwLock::new(service));
    let repair_semaphore = Arc::new(Semaphore::new(1));

    // Single serialized writer thread
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<OutgoingMessage>(256);
    let writer_thread = std::thread::spawn(move || {
        let mut out = std::io::stdout();
        while let Ok(msg) = out_rx.recv() {
            if let Err(e) = write_message(&mut out, &msg) {
                eprintln!("agon-bridge writer error: {e}");
                break;
            }
            let _ = std::io::Write::flush(&mut out);
        }
    });

    // Stdin reader thread
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<Result<RawRequest, ProtocolError>>(64);
    let _reader_thread = std::thread::spawn(move || {
        let mut stdin_lock = std::io::stdin();
        loop {
            match read_frame(&mut stdin_lock) {
                Ok(Some(frame)) => match serde_json::from_slice::<RawRequest>(&frame) {
                    Ok(req) => {
                        if req_tx.blocking_send(Ok(req)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = req_tx.blocking_send(Err(ProtocolError::Json(e)));
                        break;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    let _ = req_tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });

    let mut join_set = tokio::task::JoinSet::new();
    let is_shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));

    while let Some(req_res) = req_rx.recv().await {
        let req = match req_res {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agon-bridge stdin protocol error: {e}");
                break;
            }
        };

        if is_shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        let id = req.id;
        let op = req.op;
        let params = req.params;

        if op == "shutdown" {
            is_shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
            let val = {
                let svc = service.read().await;
                svc.shutdown()
                    .unwrap_or_else(|_| json!({ "status": "shutdown" }))
            };
            let _ = out_tx.send(OutgoingMessage::Response(Response::ok(id, val)));
            break;
        }

        let svc = Arc::clone(&service);
        let repair_sem = Arc::clone(&repair_semaphore);
        let tx = out_tx.clone();

        join_set.spawn(async move {
            let resp = match op.as_str() {
                "hello" => {
                    let s = svc.read().await;
                    match s.hello() {
                        Ok(val) => Response::ok(id, val),
                        Err(ServiceError::Tampering(v)) => {
                            Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                        }
                        Err(e) => Response::err(id, "service_error", e.to_string()),
                    }
                }
                "status" => {
                    let s = svc.read().await;
                    match s.status() {
                        Ok(val) => Response::ok(id, val),
                        Err(e) => Response::err(id, "service_error", e.to_string()),
                    }
                }
                "authorize" => {
                    let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("");
                    let detail = params.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                    let s = svc.read().await;
                    match s.authorize(action, detail) {
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
                    let check_id = params
                        .get("check_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let snap = {
                        let s = svc.read().await;
                        s.snapshot()
                    };
                    match snap.verify(check_id.as_deref()) {
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
                    let check_id = params
                        .get("check_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _permit = repair_sem.acquire().await;
                    let snap = {
                        let s = svc.read().await;
                        s.snapshot()
                    };
                    let event_tx = tx.clone();
                    let on_event = move |event_name: &str, data: serde_json::Value| {
                        let _ = event_tx.send(OutgoingMessage::Event(EventFrame {
                            event: event_name.to_string(),
                            data,
                        }));
                    };
                    match snap.execute_repair(&check_id, on_event).await {
                        Ok((outcome, new_form, new_registry, forms)) => {
                            let apply_res = {
                                let mut s = svc.write().await;
                                s.apply_repair_result(new_form, new_registry, &forms)
                            };
                            match apply_res {
                                Ok(()) => Response::ok(id, format_repair_outcome(outcome)),
                                Err(ServiceError::Tampering(v)) => Response::err(
                                    id,
                                    "tampering",
                                    format!("tampering: {}", v.join(", ")),
                                ),
                                Err(e) => Response::err(id, "service_error", e.to_string()),
                            }
                        }
                        Err(ServiceError::Tampering(v)) => {
                            Response::err(id, "tampering", format!("tampering: {}", v.join(", ")))
                        }
                        Err(ServiceError::UnknownCheck(c)) => {
                            Response::err(id, "unknown_check", format!("unknown check `{c}`"))
                        }
                        Err(e) => Response::err(id, "service_error", e.to_string()),
                    }
                }
                other => {
                    Response::err(id, "unknown_op", format!("unsupported operation `{other}`"))
                }
            };

            let _ = tx.send(OutgoingMessage::Response(resp));
        });
    }

    // Wait for running tasks
    while join_set.join_next().await.is_some() {}

    // Flush and finish writer
    drop(out_tx);
    let _ = writer_thread.join();

    Ok(())
}
