use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn overlay_cleanup_survives_capture_validation_and_javascript_failures() {
    for failure in [
        "",
        "insert",
        "mount",
        "capture",
        "check",
        "remove",
        "capture+remove",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recording = commands.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let cmd: Value = serde_json::from_str(&text).unwrap();
                recording.lock().unwrap().push(cmd.clone());
                assert_eq!(cmd["sessionId"], "original-page");
                let result = match cmd["method"].as_str().unwrap() {
                    "Page.createIsolatedWorld" => json!({"executionContextId":42}),
                    "Runtime.evaluate" if failure == "insert" => {
                        json!({"result":{"type":"object","objectId":"exception"},"exceptionDetails":{"text":"insert failed"}})
                    }
                    "Runtime.evaluate" => {
                        assert_eq!(cmd["params"]["contextId"], 42);
                        json!({"result":{"type":"object","objectId":"owned-overlay"}})
                    }
                    "Page.captureScreenshot" if failure.contains("capture") => {
                        socket.send(Message::Text(json!({"id":cmd["id"],"error":{"code":-32000,"message":"capture rejected"}}).to_string())).await.unwrap();
                        continue;
                    }
                    "Page.captureScreenshot" => json!({"data":"pixels"}),
                    "Runtime.callFunctionOn" => {
                        assert_eq!(cmd["params"]["objectId"], "owned-overlay");
                        let declaration = cmd["params"]["functionDeclaration"].as_str().unwrap();
                        if (failure == "mount" && declaration.contains("mount"))
                            || (failure == "check" && declaration.contains("check"))
                            || (failure.contains("remove") && declaration.contains("remove"))
                        {
                            json!({"result":{"type":"undefined"},"exceptionDetails":{"exception":{"description":"injected JS failure","objectId":"call-exception"}}})
                        } else {
                            json!({"result":{"type":"boolean","value":true}})
                        }
                    }
                    "Runtime.releaseObject" => json!({}),
                    unexpected => panic!("unexpected operation {unexpected}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":cmd["id"],"result":result}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&url).await.unwrap();
        let rect = Rect {
            x: 0.,
            y: 0.,
            width: 800.,
            height: 600.,
        };
        let plan = CapturePlan {
            mode: CaptureMode::Viewport,
            document: rect,
            viewport: rect,
            selector: None,
            scroll_x: 0.,
            scroll_y: 0.,
        };
        let result = capture_with_overlay(
            &client,
            &FrameContext {
                frame_id: "top".into(),
                session_id: "original-page".into(),
            },
            &plan,
            &[Drawing {
                number: 7,
                rect,
                clip: rect,
            }],
            &ScreenshotOptions::default(),
        )
        .await;
        let recorded = commands.lock().unwrap().clone();
        server.abort();
        if failure.is_empty() {
            assert_eq!(result.unwrap(), "pixels");
        } else {
            let error = result.unwrap_err();
            if failure == "capture+remove" {
                assert!(
                    error.contains("capture rejected") && error.contains("overlay cleanup"),
                    "{error}"
                );
            }
        }
        let releases: Vec<_> = recorded
            .iter()
            .filter(|c| c["method"] == "Runtime.releaseObject")
            .collect();
        assert_eq!(
            releases.len(),
            if ["mount", "check", "remove", "capture+remove"].contains(&failure) {
                2
            } else {
                1
            }
        );
        let owned_release = releases
            .iter()
            .find(|r| r["params"]["objectId"] != "call-exception")
            .unwrap();
        assert_eq!(
            owned_release["params"]["objectId"],
            if failure == "insert" {
                "exception"
            } else {
                "owned-overlay"
            }
        );
        if failure != "insert" {
            assert_eq!(
                recorded
                    .iter()
                    .filter(|c| c["method"] == "Runtime.callFunctionOn"
                        && c["params"]["functionDeclaration"]
                            .as_str()
                            .unwrap()
                            .contains("remove"))
                    .count(),
                1
            );
        }
    }
}
