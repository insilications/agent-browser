//! Read-only projection of CDP border quads into the top-level CSS viewport.
//!
//! A CDP quad is renderer-local, not necessarily document-local: same-process
//! descendants are already incorporated. Only renderer boundaries add a map.
//! Keep corners until the last step so future observation consumers can reuse
//! the math without changing input dispatch or screenshot capture semantics.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};

use super::cdp::client::CdpClient;
use super::element::{parse_ref, resolve_element_object_id, FrameContext, RefMap};
use super::frame::{collect_frame_topology, FrameTarget, FrameTopology};

// Tolerate floating-point noise, not visually significant skew or rotation.
const EPSILON: f64 = 0.0001;

fn unavailable(detail: &str) -> String {
    format!("Cannot project bounding box to top-viewport: {detail}")
}

#[derive(Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy)]
struct Quad([Point; 4]);

impl Quad {
    fn read(value: &Value) -> Result<Self, String> {
        let values = value
            .as_array()
            .filter(|v| v.len() == 8)
            .ok_or_else(|| unavailable("missing or malformed box quad"))?;
        let mut points = [Point { x: 0.0, y: 0.0 }; 4];
        for (i, point) in points.iter_mut().enumerate() {
            point.x = number(&values[i * 2])?;
            point.y = number(&values[i * 2 + 1])?;
        }
        Ok(Self(points))
    }

    fn bounds(self) -> Rect {
        let x = self.0.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
        let y = self.0.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
        Rect {
            x,
            y,
            width: self.0.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max) - x,
            height: self.0.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max) - y,
        }
    }
}

#[derive(Debug, Serialize)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug)]
struct AxisAlignedMap {
    origin: Point,
    sx: f64,
    sy: f64,
}

impl AxisAlignedMap {
    fn from_content(quad: Quad, width: f64, height: f64) -> Result<Self, String> {
        let [a, b, c, d] = quad.0;
        if !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || (a.y - b.y).abs() > EPSILON
            || (b.x - c.x).abs() > EPSILON
            || (c.y - d.y).abs() > EPSILON
            || (d.x - a.x).abs() > EPSILON
            || b.x <= a.x
            || d.y <= a.y
        {
            return Err(unavailable("unsupported iframe mapping; only translation and positive axis-aligned scaling are supported"));
        }
        let map = Self {
            origin: a,
            sx: (b.x - a.x) / width,
            sy: (d.y - a.y) / height,
        };
        if !map.sx.is_finite() || !map.sy.is_finite() {
            return Err(unavailable("nonfinite iframe scale"));
        }
        Ok(map)
    }

    fn apply(&self, quad: Quad) -> Result<Quad, String> {
        let result = Quad(quad.0.map(|p| Point {
            x: self.origin.x + p.x * self.sx,
            y: self.origin.y + p.y * self.sy,
        }));
        if result
            .0
            .iter()
            .any(|p| !p.x.is_finite() || !p.y.is_finite())
        {
            return Err(unavailable("nonfinite projected geometry"));
        }
        Ok(result)
    }
}

fn number(value: &Value) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| unavailable("missing or nonfinite geometry metric"))
}

fn frame_chain(topology: &FrameTopology, frame_id: &str) -> Result<Vec<FrameTarget>, String> {
    if topology.incomplete {
        return Err(unavailable(
            "frame topology is incomplete; retry the command",
        ));
    }
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut id = frame_id;
    loop {
        let frame = topology
            .frames
            .get(id)
            .filter(|_| visited.insert(id))
            .ok_or_else(|| unavailable("cannot establish frame ancestry"))?;
        if frame.loader_id.as_deref().is_none_or(str::is_empty) {
            return Err(unavailable("cannot establish document identity"));
        }
        chain.push(frame.clone());
        if id == topology.top_frame_id {
            return Ok(chain);
        }
        id = frame
            .parent_id
            .as_deref()
            .ok_or_else(|| unavailable("cannot establish the parent frame"))?;
    }
}

fn viewport_supported(metrics: &Value) -> Result<(), String> {
    let viewport = &metrics["cssVisualViewport"];
    if (number(&viewport["scale"])? - 1.0).abs() > EPSILON
        || (number(&viewport["zoom"])? - 1.0).abs() > EPSILON
        || number(&viewport["offsetX"])?.abs() > EPSILON
        || number(&viewport["offsetY"])?.abs() > EPSILON
    {
        return Err(unavailable(
            "browser/pinch zoom and displaced visual viewports are unsupported",
        ));
    }
    Ok(())
}

// Placement comes from CDP, not parsed CSS matrices. Read fractional used
// content sizes here: BoxModel.width/height are rounded offset dimensions and
// child innerWidth/innerHeight are rounded too. Inspect composed ancestors so
// a rotated wrapper, shadow host, individual transform, or perspective cannot
// be mistaken for a supported embedding merely because its quad looks square.
const METRICS_JS: &str = r#"function(embedding) {
    if (!this.isConnected || !this.ownerDocument.defaultView)
        return {error:'element or iframe owner is detached'};
    const win = this.ownerDocument.defaultView;
    const parent = el => el.assignedSlot || el.parentElement || el.getRootNode().host;
    for (let el = this; el; el = parent(el)) {
        const s = win.getComputedStyle(el);
        if (s.zoom !== 'normal' && Number(s.zoom) !== 1)
            return {error:'CSS zoom is unsupported'};
        if (!embedding) continue;
        if (s.perspective !== 'none' || s.transformStyle !== 'flat' || s.offsetPath !== 'none')
            return {error:'perspective, 3D and motion-path iframe transforms are unsupported'};
        if (s.transform !== 'none') {
            const m = new win.DOMMatrixReadOnly(s.transform);
            if (!m.is2D || Math.abs(m.b) > 1e-8 || Math.abs(m.c) > 1e-8 || m.a <= 0 || m.d <= 0)
                return {error:'iframe rotation, skew, reflection and 3D transforms are unsupported'};
        }
        if (s.rotate !== 'none' && !/^0(?:deg|rad|grad|turn)?$/.test(s.rotate))
            return {error:'individual iframe rotation is unsupported'};
        if (s.scale !== 'none') {
            const scale = s.scale.split(/\s+/).map(Number);
            if (scale.some(n => !Number.isFinite(n) || n <= 0) || (scale.length > 2 && scale[2] !== 1))
                return {error:'iframe reflection and 3D scaling are unsupported'};
        }
        if (s.translate !== 'none') {
            const parts = s.translate.split(/\s+/);
            if (parts.length > 2 && !/^0(?:px)?$/.test(parts[2]))
                return {error:'3D iframe translation is unsupported'};
        }
    }
    if (!embedding) return {};
    const s = win.getComputedStyle(this);
    const px = value => /^-?(?:\d+(?:\.\d*)?|\.\d+)px$/.test(value) ? parseFloat(value) : NaN;
    let width = px(s.width), height = px(s.height);
    if (s.boxSizing === 'border-box') {
        width -= px(s.borderLeftWidth) + px(s.borderRightWidth) + px(s.paddingLeft) + px(s.paddingRight);
        height -= px(s.borderTopWidth) + px(s.borderBottomWidth) + px(s.paddingTop) + px(s.paddingBottom);
    }
    if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0)
        return {error:'iframe has no measurable positive content size'};
    return {width,height};
}"#;

async fn element_metrics(
    client: &CdpClient,
    session: &str,
    object: &str,
    embedding: bool,
) -> Result<Value, String> {
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId":object, "functionDeclaration":METRICS_JS,
                "arguments":[{"value":embedding}], "returnByValue":true,
            })),
            Some(session),
        )
        .await?;
    if result.get("exceptionDetails").is_some() {
        return Err(unavailable("could not inspect element layout metrics"));
    }
    let value = result
        .pointer("/result/value")
        .filter(|v| v.is_object())
        .ok_or_else(|| unavailable("element layout metrics are unavailable"))?;
    if let Some(message) = value["error"].as_str() {
        return Err(unavailable(message));
    }
    Ok(value.clone())
}

async fn box_model(client: &CdpClient, session: &str, object: &str) -> Result<Value, String> {
    let result = client
        .send_command(
            "DOM.getBoxModel",
            Some(json!({"objectId":object})),
            Some(session),
        )
        .await?;
    result
        .get("model")
        .cloned()
        .ok_or_else(|| unavailable("element has no measurable box model"))
}

/// A single measurement after selected-frame preparation. No action replay,
/// geometry cache, scrolling, or transfer of handles to replacement renderers.
pub(super) async fn top_viewport_box(
    client: &CdpClient,
    page_session: &str,
    refs: &RefMap,
    selector: &str,
    selected: Option<&FrameContext>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Value, String> {
    let topology = collect_frame_topology(client, page_session, iframe_sessions).await?;
    let frame_id = if let Some(id) = parse_ref(selector) {
        refs.get(&id)
            .ok_or_else(|| format!("Unknown ref: {id}"))?
            .frame_id
            .as_deref()
    } else {
        selected.map(|frame| frame.frame_id.as_str())
    }
    .unwrap_or(&topology.top_frame_id);
    let chain = frame_chain(&topology, frame_id)?;
    let (object, session) =
        resolve_element_object_id(client, page_session, refs, selector, iframe_sessions).await?;
    let mut handles = vec![(session.clone(), object.clone())];
    // Keep cleanup outside the fallible measurement so both successful and
    // rejected projections release every temporary owner/element handle.
    let result = async {
        if session != chain[0].session_id {
            return Err(unavailable(
                "element renderer changed; take a fresh snapshot and retry",
            ));
        }
        let mut sessions = HashSet::new();
        for frame in &chain {
            if sessions.insert(&frame.session_id) {
                viewport_supported(
                    &client
                        .send_command_no_params("Page.getLayoutMetrics", Some(&frame.session_id))
                        .await?,
                )?;
            }
        }
        element_metrics(client, &session, &object, false).await?;
        let mut quad = Quad::read(&box_model(client, &session, &object).await?["border"])?;
        for pair in chain.windows(2) {
            let (child, parent) = (&pair[0], &pair[1]);
            let owner = client
                .send_command(
                    "DOM.getFrameOwner",
                    Some(json!({"frameId":child.frame_id})),
                    Some(&parent.session_id),
                )
                .await?;
            let backend = owner["backendNodeId"]
                .as_i64()
                .ok_or_else(|| unavailable("iframe owner is unavailable"))?;
            let resolved = client
                .send_command(
                    "DOM.resolveNode",
                    Some(json!({"backendNodeId":backend})),
                    Some(&parent.session_id),
                )
                .await?;
            let owner_object = resolved
                .pointer("/object/objectId")
                .and_then(Value::as_str)
                .ok_or_else(|| unavailable("iframe owner has no remote handle"))?
                .to_string();
            handles.push((parent.session_id.clone(), owner_object.clone()));
            let dimensions =
                element_metrics(client, &parent.session_id, &owner_object, true).await?;
            let content = Quad::read(
                &box_model(client, &parent.session_id, &owner_object).await?["content"],
            )?;
            let mapping = AxisAlignedMap::from_content(
                content,
                number(&dimensions["width"])?,
                number(&dimensions["height"])?,
            )?;
            if child.session_id != parent.session_id {
                quad = mapping.apply(quad)?;
            }
        }
        let current = collect_frame_topology(client, page_session, iframe_sessions).await?;
        if current.top_frame_id != topology.top_frame_id
            || frame_chain(&current, frame_id)? != chain
        {
            return Err(unavailable(
                "frame document or renderer changed during measurement; retry the command",
            ));
        }
        serde_json::to_value(quad.bounds()).map_err(|e| unavailable(&e.to_string()))
    }
    .await;
    for (sid, handle) in handles {
        let _ = client
            .send_command(
                "Runtime.releaseObject",
                Some(json!({"objectId":handle})),
                Some(&sid),
            )
            .await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(values: [f64; 8]) -> Quad {
        Quad::read(&json!(values)).unwrap()
    }

    #[test]
    fn projection_composes_corners_and_keeps_fractional_css_units() {
        let target = quad([20.25, 30.5, 100.25, 30.5, 100.25, 70.5, 20.25, 70.5]);
        let inner = AxisAlignedMap::from_content(
            quad([35., 45., 735., 45., 735., 545., 35., 545.]),
            350.,
            250.,
        )
        .unwrap();
        let outer = AxisAlignedMap::from_content(
            quad([250., 170., 1250., 170., 1250., 570., 250., 570.]),
            500.,
            400.,
        )
        .unwrap();
        let result = outer.apply(inner.apply(target).unwrap()).unwrap().bounds();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"x":401.,"y":276.,"width":320.,"height":80.})
        );
        let fractional = AxisAlignedMap::from_content(
            quad([274., 185.75, 899.625, 185.75, 899.625, 786.5, 274., 786.5]),
            500.5,
            400.5,
        )
        .unwrap();
        assert_eq!(fractional.sx, 1.25);
        assert_eq!(fractional.sy, 1.5);
    }

    #[test]
    fn projection_rejects_unsupported_and_malformed_mappings() {
        for values in [
            [0., 0., 100., 1., 100., 100., 0., 100.],   // skew
            [100., 0., 0., 0., 0., 100., 100., 100.],   // reflection
            [0., 0., 100., 0., 80., 100., 20., 100.],   // perspective
            [0., 0., 0., 0., 0., 100., 0., 100.],       // degenerate
            [0., 0., 0., 100., -100., 100., -100., 0.], // rotation
        ] {
            assert!(AxisAlignedMap::from_content(quad(values), 100., 100.).is_err());
        }
        for width in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(AxisAlignedMap::from_content(
                quad([0., 0., 100., 0., 100., 100., 0., 100.]),
                width,
                100.
            )
            .is_err());
        }
        assert!(Quad::read(&json!([0, 1])).is_err());
        assert!(Quad::read(&json!([null, 0, 1, 0, 1, 1, 0, 1])).is_err());
    }

    #[test]
    fn projection_does_not_clip_offscreen_or_zero_area_bounds() {
        let rect = quad([-10., -20., 0., -20., 0., -20., -10., -20.]).bounds();
        assert_eq!(
            serde_json::to_value(rect).unwrap(),
            json!({"x":-10.,"y":-20.,"width":10.,"height":0.})
        );
    }

    #[test]
    fn viewport_guard_requires_known_unzoomed_css_space() {
        let metrics = json!({"cssVisualViewport":{"scale":1,"zoom":1,"offsetX":0,"offsetY":0}});
        assert!(viewport_supported(&metrics).is_ok());
        for key in ["scale", "zoom", "offsetX", "offsetY"] {
            let mut changed = metrics.clone();
            changed["cssVisualViewport"][key] = json!(2);
            assert!(viewport_supported(&changed).is_err());
            changed["cssVisualViewport"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(viewport_supported(&changed).is_err());
        }
    }

    #[test]
    fn ancestry_requires_complete_document_and_renderer_identity() {
        let frame = |id: &str, parent: Option<&str>| FrameTarget {
            frame_id: id.into(),
            session_id: "page".into(),
            parent_id: parent.map(str::to_string),
            loader_id: Some(format!("{id}-doc")),
        };
        let mut topology = FrameTopology {
            top_frame_id: "top".into(),
            incomplete: false,
            frames: HashMap::from([
                ("top".into(), frame("top", None)),
                ("inner".into(), frame("inner", Some("top"))),
            ]),
        };
        assert_eq!(frame_chain(&topology, "inner").unwrap().len(), 2);
        topology.frames.get_mut("inner").unwrap().parent_id = Some("missing".into());
        assert!(frame_chain(&topology, "inner").is_err());
        topology.frames.get_mut("inner").unwrap().parent_id = Some("inner".into());
        assert!(frame_chain(&topology, "inner").is_err());
        topology.frames.get_mut("inner").unwrap().parent_id = Some("top".into());
        topology.frames.get_mut("inner").unwrap().loader_id = None;
        assert!(frame_chain(&topology, "inner").is_err());
        topology.incomplete = true;
        assert!(frame_chain(&topology, "top").is_err());
    }

    // A real CDP transport lets the tests assert routing, cleanup and the
    // absence of mutation without relying on browser navigation timing.
    async fn mock_projection(
        dedicated: bool,
        failure: &str,
    ) -> (Result<Value, String>, Vec<Value>) {
        use super::super::element::RefContext;
        use futures_util::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = commands.clone();
        let failure = failure.to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut top_queries = 0;
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                recorded.lock().unwrap().push(command.clone());
                let params = &command["params"];
                let result = match command["method"].as_str().unwrap() {
                    "Page.getFrameTree" => {
                        if command["sessionId"] == "page" {
                            top_queries += 1;
                        }
                        let inner = json!({"frame":{"id":"inner","parentId":"outer","loaderId":"inner-doc"}});
                        let outer = json!({"frame":{"id":"outer","parentId":if failure == "ancestry" {"missing"} else {"top"},"loaderId":if failure == "navigation" && top_queries > 1 {"new-doc"} else {"outer-doc"}},"childFrames":[inner]});
                        if command["sessionId"] == "page" {
                            json!({"frameTree":{"frame":{"id":"top","loaderId":"top-doc"},"childFrames":[outer]}})
                        } else {
                            json!({"frameTree":outer})
                        }
                    }
                    "DOM.resolveNode" => {
                        json!({"object":{"type":"object","objectId":match params["backendNodeId"].as_i64().unwrap() {1=>"target",2=>"inner-owner",3=>"outer-owner",_=>panic!("unexpected node")}}})
                    }
                    "Page.getLayoutMetrics" => {
                        json!({"cssVisualViewport":{"scale":1,"zoom":1,"offsetX":0,"offsetY":0}})
                    }
                    "Runtime.callFunctionOn" => {
                        assert_eq!(params["functionDeclaration"], METRICS_JS);
                        let value = match params["objectId"].as_str().unwrap() {
                            "target" => json!({}),
                            _ if failure == "style" => json!({"error":"unsupported style"}),
                            "inner-owner" => json!({"width":350,"height":250}),
                            "outer-owner" => json!({"width":500,"height":400}),
                            _ => panic!("unexpected object"),
                        };
                        json!({"result":{"type":"object","value":value}})
                    }
                    "DOM.getBoxModel" => match params["objectId"].as_str().unwrap() {
                        "target" if dedicated => {
                            json!({"model":{"border":[55,75,135,75,135,115,55,115]}})
                        }
                        "target" => json!({"model":{"border":[305,245,385,245,385,285,305,285]}}),
                        "inner-owner" if dedicated => {
                            json!({"model":{"content":[35,45,385,45,385,295,35,295]}})
                        }
                        "inner-owner" => {
                            json!({"model":{"content":[285,215,635,215,635,465,285,465]}})
                        }
                        "outer-owner" => {
                            json!({"model":{"content":[250,170,750,170,750,570,250,570]}})
                        }
                        _ => panic!("unexpected object"),
                    },
                    "DOM.getFrameOwner" if failure == "owner" => {
                        socket.send(Message::Text(json!({"id":command["id"],"error":{"code":-32000,"message":"No frame owner"}}).to_string())).await.unwrap();
                        continue;
                    }
                    "DOM.getFrameOwner" => {
                        json!({"backendNodeId":if params["frameId"] == "inner" {2} else {3}})
                    }
                    "Runtime.releaseObject" => json!({}),
                    unexpected => panic!("unexpected CDP operation: {unexpected}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":command["id"],"result":result}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&url).await.unwrap();
        let mut refs = RefMap::new();
        refs.add_with_frame(
            "e1".into(),
            Some(1),
            "button",
            "Target",
            None,
            RefContext {
                frame_id: Some("inner"),
                session_id: Some(if dedicated { "oopif" } else { "page" }),
            },
        );
        let sessions = if dedicated {
            HashMap::from([("outer".into(), "oopif".into())])
        } else {
            HashMap::new()
        };
        let selected = FrameContext {
            frame_id: "top".into(),
            session_id: "page".into(),
        };
        let result =
            top_viewport_box(&client, "page", &refs, "@e1", Some(&selected), &sessions).await;
        let recorded = commands.lock().unwrap().clone();
        server.abort();
        (result, recorded)
    }

    #[tokio::test]
    async fn projection_respects_ref_provenance_and_renderer_boundaries_without_side_effects() {
        for dedicated in [false, true] {
            let (result, commands) = mock_projection(dedicated, "").await;
            assert_eq!(
                result.unwrap(),
                json!({"x":305.,"y":245.,"width":80.,"height":40.})
            );
            let target = commands
                .iter()
                .find(|cmd| cmd["method"] == "DOM.resolveNode")
                .unwrap();
            assert_eq!(
                target["sessionId"],
                if dedicated { "oopif" } else { "page" }
            );
            let owners: Vec<_> = commands
                .iter()
                .filter(|cmd| cmd["method"] == "DOM.getFrameOwner")
                .collect();
            assert_eq!(owners.len(), 2);
            assert_eq!(
                owners[0]["sessionId"],
                if dedicated { "oopif" } else { "page" }
            );
            assert_eq!(owners[1]["sessionId"], "page");
            assert_eq!(
                commands
                    .iter()
                    .filter(|cmd| cmd["method"] == "Runtime.releaseObject")
                    .count(),
                3
            );
            // The mock rejects every command outside this read-only protocol.
        }
    }

    #[tokio::test]
    async fn projection_fails_closed_and_releases_handles_on_partial_failure() {
        for (failure, handles) in [
            ("ancestry", 0),
            ("owner", 1),
            ("style", 2),
            ("navigation", 3),
        ] {
            let (result, commands) = mock_projection(true, failure).await;
            assert!(result.is_err(), "{failure}: {result:?}");
            let released: Vec<_> = commands
                .iter()
                .filter(|cmd| cmd["method"] == "Runtime.releaseObject")
                .map(|cmd| cmd["params"]["objectId"].as_str().unwrap())
                .collect();
            assert_eq!(released.len(), handles, "{failure}: {released:?}");
            assert_eq!(released.iter().collect::<HashSet<_>>().len(), handles);
        }
    }
}
