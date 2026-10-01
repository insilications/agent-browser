//! Delayed real transport: shared acquisition, routing, failure classification,
//! bounded target concurrency, and handle ownership are observable here.
use super::*;
use crate::native::element::RefContext;
use futures_util::{stream::FuturesUnordered, SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

async fn batch(failure: &str) -> (Result<Vec<ProjectedRef>, String>, Vec<Value>, usize) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let records = Arc::new(Mutex::new((Vec::new(), 0)));
    let recording = records.clone();
    let failure = failure.to_string();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut source) = socket.split();
        let mut pending = FuturesUnordered::new();
        let mut queries: HashMap<String, usize> = HashMap::new();
        loop {
            tokio::select! {
                Some(Ok(Message::Text(text))) = source.next() => {
                    let cmd:Value=serde_json::from_str(&text).unwrap();
                    recording.lock().unwrap().0.push(cmd.clone());
                    let method=cmd["method"].as_str().unwrap();
                    let p=&cmd["params"];
                    let object=p["objectId"].as_str().unwrap_or("");
                    let count=queries.entry(format!("{method}/{object}/{}",cmd["sessionId"])).or_default();
                    *count+=1;
                    let mut error=None;
                    let result=match method {
                        "Page.getFrameTree" => {
                            let inner=json!({"frame":{"id":"inner","parentId":"outer","loaderId":if failure=="navigation" && *count>1 {"replacement"} else {"inner-doc"}}});
                            let outer=json!({"frame":{"id":"outer","parentId":"top","loaderId":"outer-doc"},"childFrames":[inner]});
                            json!({"frameTree":if cmd["sessionId"]=="page" {json!({"frame":{"id":"top","loaderId":"top-doc"},"childFrames":[outer]})}else{outer}})
                        }
                        "DOM.resolveNode" => {
                            let id=p["backendNodeId"].as_i64().unwrap();
                            assert_eq!(cmd["sessionId"], if id==1002 {"page"}else{"oopif"});
                            if failure=="resolve" && id==7 {error=Some("required resolve failed");}
                            json!({"object":{"type":"object","objectId":format!("node{id}")}})
                        }
                        "DOM.getFrameOwner" => {
                            let inner=p["frameId"]=="inner";
                            assert_eq!(cmd["sessionId"],if inner {"oopif"} else {"page"});
                            if failure=="owner" {error=Some("owner unavailable");}
                            json!({"backendNodeId":if inner {1001}else{1002}})
                        }
                        "Page.getLayoutMetrics" => json!({"cssVisualViewport":{"scale":1,"zoom":1,"offsetX":0,"offsetY":0,"clientWidth":800,"clientHeight":600},"cssContentSize":{"x":0,"y":0,"width":800,"height":2000}}),
                        "Page.createIsolatedWorld" => json!({"executionContextId":if p["frameId"]=="inner" {2}else{1}}),
                        "Runtime.evaluate" => {
                            assert_eq!(p["expression"],"document.documentElement");
                            json!({"result":{"type":"object","objectId":format!("doc{}",p["contextId"].as_i64().unwrap_or(0))}})
                        }
                        "Runtime.callFunctionOn" => {
                            let value=if p["functionDeclaration"]==DOCUMENT_METRICS_JS {
                                json!({"x":0,"y":if failure=="scroll" && *count>1 {1}else{0},"width":800,"height":600})
                            }else {
                                assert_eq!(p["functionDeclaration"],METRICS_JS);
                                match object {
                                    "node1001"=>json!({"width":350,"height":250,"boxSizing":"content-box"}),
                                    "node1002"=>json!({"width":520,"height":420,"boxSizing":"border-box"}),
                                    _=>json!({"noLayout":failure=="no-layout" && object=="node7"})
                                }
                            };
                            json!({"result":{"type":"object","value":value}})
                        }
                        "DOM.getBoxModel" => match object {
                            "node1001"=>json!({"model":{"content":[35,45,385,45,385,295,35,295]}}),
                            "node1002"=>{
                                let x=if failure=="mapping" && *count>1 {251}else{250};
                                json!({"model":{"content":[x,170,750,170,750,570,x,570],"border":[240,160,760,160,760,580,240,580]}})
                            },
                            _=>{
                                assert_eq!(cmd["sessionId"],"oopif");
                                if failure=="box" && object=="node7" {error=Some("required box failed");}
                                json!({"model":{"border":[55.25,75.5,135.25,75.5,135.25,115.5,55.25,115.5]}})
                            }
                        },
                        "Runtime.releaseObject" =>json!({}),
                        unexpected=>panic!("unexpected mutation or acquisition: {unexpected}")
                    };
                    let response=if let Some(message)=error {json!({"id":cmd["id"],"error":{"code":-32000,"message":message}})}else{json!({"id":cmd["id"],"result":result})};
                    pending.push(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        response
                    });
                    let mut record=recording.lock().unwrap();
                    record.1=record.1.max(pending.len());
                }
                Some(response)=pending.next(), if !pending.is_empty()=>{
                    if sink.send(Message::Text(response.to_string())).await.is_err() {break;}
                }
                else=>break,
            }
        }
    });
    let client = CdpClient::connect(&url).await.unwrap();
    let sessions = HashMap::from([("outer".into(), "oopif".into())]);
    let mut refs = RefMap::new();
    for id in 1..=50 {
        refs.add_with_frame(
            format!("e{id}"),
            Some(id),
            "button",
            "Target",
            None,
            RefContext {
                frame_id: Some("inner"),
                session_id: Some("oopif"),
            },
        );
    }
    let mut pass = ProjectionPass::new(&client, "page", &sessions)
        .await
        .unwrap();
    let result: ProjectionResult<Vec<ProjectedRef>> = async {
        let targets = pass.resolve_refs(&refs).await?;
        pass.prepare(true).await?;
        let measured = pass.measure_refs(&targets).await?;
        pass.validate(true).await?;
        assert_eq!(pass.document_signature().as_array().unwrap().len(), 3);
        Ok(measured)
    }
    .await;
    pass.release().await;
    let (commands, max_pending) = records.lock().unwrap().clone();
    server.abort();
    (result.map_err(|e| e.to_string()), commands, max_pending)
}

#[tokio::test]
async fn projection_batch_shares_ancestry_bounds_concurrency_and_preserves_order() {
    let (result, commands, max_pending) = batch("").await;
    let measured = result.unwrap();
    assert_eq!(measured.len(), 50);
    for (i, target) in measured.iter().enumerate() {
        assert_eq!(target.ref_id, format!("e{}", i + 1));
        assert_eq!(
            serde_json::to_value(target.geometry.bounds()).unwrap(),
            json!({"x":305.25,"y":245.5,"width":80.,"height":40.})
        );
        assert_eq!(target.geometry.embedding_clips.len(), 2);
        assert_eq!(
            target.geometry.embedding_clips[0],
            Rect {
                x: 285.,
                y: 215.,
                width: 350.,
                height: 250.
            }
        );
    }
    assert!(
        max_pending > 1 && max_pending <= MEASUREMENT_CONCURRENCY,
        "pending={max_pending}"
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| c["method"] == "DOM.getFrameOwner")
            .count(),
        2
    );
    for id in ["node1001", "node1002"] {
        assert_eq!(
            commands
                .iter()
                .filter(|c| c["method"] == "DOM.getBoxModel" && c["params"]["objectId"] == id)
                .count(),
            2,
            "initial plus final validation only"
        );
    }
    assert_eq!(
        commands
            .iter()
            .filter(|c| c["method"] == "Runtime.releaseObject")
            .count(),
        55
    );
}

#[tokio::test]
async fn projection_batch_fails_closed_without_leaking_partial_acquisitions() {
    for failure in [
        "resolve",
        "owner",
        "box",
        "navigation",
        "scroll",
        "mapping",
        "no-layout",
    ] {
        let (result, commands, _) = batch(failure).await;
        if failure == "no-layout" {
            assert_eq!(result.unwrap().len(), 49);
            assert!(!commands
                .iter()
                .any(|c| c["method"] == "DOM.getBoxModel" && c["params"]["objectId"] == "node7"));
        } else {
            assert!(result.is_err(), "{failure}");
        }
        let acquired = commands
            .iter()
            .filter(|c| {
                (c["method"] == "DOM.resolveNode"
                    && !(failure == "resolve" && c["params"]["backendNodeId"] == 7))
                    || c["method"] == "Runtime.evaluate"
            })
            .count();
        let releases: HashSet<_> = commands
            .iter()
            .filter(|c| c["method"] == "Runtime.releaseObject")
            .map(|c| {
                (
                    c["sessionId"].to_string(),
                    c["params"]["objectId"].to_string(),
                )
            })
            .collect();
        assert_eq!(releases.len(), acquired, "{failure}");
    }
}
