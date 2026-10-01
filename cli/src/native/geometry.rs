//! Read-only projection of CDP border quads into the top-level CSS viewport.
//!
//! A CDP quad is renderer-local, not necessarily document-local: same-process
//! descendants are already incorporated. Only renderer boundaries add a map.
//! Keep corners until projection is complete. Capture origins and drawing
//! clips belong to screenshots, not input dispatch or the get-box contract.

use std::collections::{HashMap, HashSet};

use futures_util::future::join_all;
use serde::Serialize;
use serde_json::{json, Value};

use super::cdp::client::CdpClient;
use super::element::{
    evaluate_in_context, frame_execution_context, parse_ref, resolve_element_object_id,
    FrameContext, RefEntry, RefMap,
};
use super::frame::{collect_frame_topology, FrameTarget, FrameTopology};

// Tolerate floating-point noise, not visually significant skew or rotation.
pub(super) const EPSILON: f64 = 0.0001;
pub(super) const MEASUREMENT_CONCURRENCY: usize = 16;

fn unavailable(detail: &str) -> String {
    format!("Cannot project bounding box to top-viewport: {detail}")
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Unrounded CSS geometry. Its coordinate space is supplied by the caller;
/// screenshot capture conversion never changes the full element dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn read(value: &Value) -> Result<Self, String> {
        Ok(Self {
            x: number(&value["x"])?,
            y: number(&value["y"])?,
            width: number(&value["width"])?,
            height: number(&value["height"])?,
        })
    }

    pub fn positive(self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    pub fn translated(self, x: f64, y: f64) -> Self {
        Self {
            x: self.x + x,
            y: self.y + y,
            ..self
        }
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let rect = Self {
            x,
            y,
            width: (self.x + self.width).min(other.x + other.width) - x,
            height: (self.y + self.height).min(other.y + other.height) - y,
        };
        rect.positive().then_some(rect)
    }
}

#[derive(Debug, Clone, Copy)]
struct AxisAlignedMap {
    origin: Point,
    sx: f64,
    sy: f64,
}

impl AxisAlignedMap {
    fn from_quad(quad: Quad, width: f64, height: f64) -> Result<Self, String> {
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

    fn from_box_model(model: &Value, dimensions: &Value) -> Result<Self, String> {
        let width = number(&dimensions["width"])?;
        let height = number(&dimensions["height"])?;
        let content = Quad::read(&model["content"])?;
        // Always validate the content quad, including its positive size, and
        // use its origin for the child viewport regardless of box-sizing.
        let mut map = Self::from_quad(content, width, height)?;
        if dimensions["boxSizing"] == "border-box" {
            // Compare like boxes to recover scale. Subtracting computed CSS
            // padding from a used border size mixes unquantized and layout-
            // rounded values, inventing scale when the content box is small.
            let border = Self::from_quad(Quad::read(&model["border"])?, width, height)?;
            map.sx = border.sx;
            map.sy = border.sy;
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

// Placement comes from CDP, not parsed CSS matrices. Read fractional used sizes
// in their CSS sizing box: BoxModel.width/height and child innerWidth/innerHeight
// are rounded. Inspect composed ancestors so a rotated wrapper, shadow host,
// individual transform, or perspective cannot be mistaken for a supported
// embedding merely because its quad looks square.
const METRICS_JS: &str = r#"function(embedding) {
    if (!this.isConnected || !this.ownerDocument.defaultView) {
        return { error: 'element or iframe owner is detached' };
    }
    const win = this.ownerDocument.defaultView;
    const parent = (el) => el.assignedSlot || el.parentElement || el.getRootNode().host;
    for (let el = this; el; el = parent(el)) {
        const s = win.getComputedStyle(el);
        if (s.zoom !== 'normal' && Number(s.zoom) !== 1) {
            return { error: 'CSS zoom is unsupported' };
        }
        if (!embedding) {
            continue;
        }
        if (s.perspective !== 'none' || s.transformStyle !== 'flat' || s.offsetPath !== 'none') {
            return { error: 'perspective, 3D and motion-path iframe transforms are unsupported' };
        }
        if (s.transform !== 'none') {
            const m = new win.DOMMatrixReadOnly(s.transform);
            if (!m.is2D || Math.abs(m.b) > 1e-8 || Math.abs(m.c) > 1e-8 || m.a <= 0 || m.d <= 0) {
                return {
                    error: 'iframe rotation, skew, reflection and 3D transforms are unsupported',
                };
            }
        }
        if (s.rotate !== 'none' && !/^0(?:deg|rad|grad|turn)?$/.test(s.rotate)) {
            return { error: 'individual iframe rotation is unsupported' };
        }
        if (s.scale !== 'none') {
            const scale = s.scale.split(/\s+/).map(Number);
            if (
                scale.some((n) => !Number.isFinite(n) || n <= 0) ||
                (scale.length > 2 && scale[2] !== 1)
            ) {
                return { error: 'iframe reflection and 3D scaling are unsupported' };
            }
        }
        if (s.translate !== 'none') {
            const parts = s.translate.split(/\s+/);
            if (parts.length > 2 && !/^0(?:px)?$/.test(parts[2])) {
                return { error: '3D iframe translation is unsupported' };
            }
        }
    }
    if (!embedding) {
        return { noLayout: this.getClientRects().length === 0 };
    }
    const s = win.getComputedStyle(this);
    const px = (value) => (/^-?(?:\d+(?:\.\d*)?|\.\d+)px$/.test(value) ? parseFloat(value) : NaN);
    const width = px(s.width),
        height = px(s.height);
    if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) {
        return { error: 'iframe has no measurable positive size' };
    }
    return { width, height, boxSizing: s.boxSizing };
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
        release_exception_objects(
            client,
            session,
            result.pointer("/result/objectId").and_then(Value::as_str),
            result
                .pointer("/exceptionDetails/exception/objectId")
                .and_then(Value::as_str),
        )
        .await;
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

/// A thrown Runtime exception can retain an object even with returnByValue.
/// Release both protocol locations without double-releasing the same handle.
pub(super) async fn release_exception_objects(
    client: &CdpClient,
    session: &str,
    result: Option<&str>,
    exception: Option<&str>,
) {
    let mut seen = HashSet::new();
    for object in [result, exception].into_iter().flatten() {
        if seen.insert(object) {
            let _ = client
                .send_command(
                    "Runtime.releaseObject",
                    Some(json!({"objectId":object})),
                    Some(session),
                )
                .await;
        }
    }
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

/// Acquisition errors are never confused with a confirmed non-rendering node.
/// Existing public commands retain their normal string error envelope.
#[derive(Debug)]
pub(super) enum ProjectionError {
    Unavailable(String),
    Changed(String),
}

impl From<String> for ProjectionError {
    fn from(message: String) -> Self {
        Self::Unavailable(message)
    }
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => f.write_str(message),
            Self::Changed(detail) => f.write_str(&unavailable(&format!(
                "{detail} changed during measurement; retry the command"
            ))),
        }
    }
}

type ProjectionResult<T> = Result<T, ProjectionError>;

#[derive(Debug, Clone)]
struct RemoteHandle {
    session: String,
    object: String,
}

/// A selector target pinned before an annotation snapshot refreshes the refs.
/// Handles never migrate to a replacement document or renderer.
#[derive(Debug, Clone)]
pub(super) struct ResolvedTarget {
    pub frame_id: String,
    handle: RemoteHandle,
}

#[derive(Debug)]
pub(super) struct ProjectedGeometry {
    border: Quad,
    /// Top-viewport content boundaries, including same-process embeddings.
    pub embedding_clips: Vec<Rect>,
}

impl ProjectedGeometry {
    pub fn bounds(&self) -> Rect {
        self.border.bounds()
    }
}

#[derive(Debug)]
pub(super) enum Measurement {
    Box(ProjectedGeometry),
    NoLayout,
}

pub(super) struct ProjectedRef {
    pub ref_id: String,
    pub entry: RefEntry,
    pub geometry: ProjectedGeometry,
}

struct Embedding {
    handle: RemoteHandle,
    dimensions: Value,
    model: Value,
    mapping: AxisAlignedMap,
    content: Quad,
}

struct DocumentProbe {
    handle: RemoteHandle,
    metrics: Value,
}

// These are document-local observations, not scale denominators. In particular,
// innerWidth is never used to reconstruct an iframe's fractional sizing box.
const DOCUMENT_METRICS_JS: &str = r#"function() {
    if (!this.isConnected || !this.ownerDocument.defaultView) return null;
    const w = this.ownerDocument.defaultView;
    return {x:w.scrollX, y:w.scrollY, width:w.innerWidth, height:w.innerHeight};
}"#;

async fn document_metrics(client: &CdpClient, handle: &RemoteHandle) -> ProjectionResult<Value> {
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId":handle.object, "functionDeclaration":DOCUMENT_METRICS_JS,
                "returnByValue":true,
            })),
            Some(&handle.session),
        )
        .await?;
    if result.get("exceptionDetails").is_some() {
        release_exception_objects(
            client,
            &handle.session,
            result.pointer("/result/objectId").and_then(Value::as_str),
            result
                .pointer("/exceptionDetails/exception/objectId")
                .and_then(Value::as_str),
        )
        .await;
        return Err(unavailable("could not inspect document scrolling").into());
    }
    let value = result
        .pointer("/result/value")
        .ok_or_else(|| unavailable("document metrics unavailable"))?;
    for key in ["x", "y", "width", "height"] {
        number(&value[key])?;
    }
    Ok(value.clone())
}

/// Compare browser-produced geometry with numerical tolerance. This is a final
/// validation pass, not stability polling or an atomic layout snapshot.
fn metrics_match(before: &Value, after: &Value) -> bool {
    match (before, after) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .is_some_and(|(a, b)| a.is_finite() && b.is_finite() && (a - b).abs() <= EPSILON),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| metrics_match(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| metrics_match(a, b)))
        }
        _ => before == after,
    }
}

/// One operation's shared acquisition state. Nothing survives the command.
/// Both get-box and annotations use the same quads and sizing-box calculation;
/// annotations additionally validate scrolling/embedding geometry across capture.
pub(super) struct ProjectionPass<'a> {
    client: &'a CdpClient,
    page_session: &'a str,
    iframe_sessions: &'a HashMap<String, String>,
    topology: FrameTopology,
    chains: HashMap<String, Vec<FrameTarget>>,
    viewports: HashMap<String, Value>,
    embeddings: HashMap<String, Embedding>,
    documents: HashMap<String, DocumentProbe>,
    handles: Vec<RemoteHandle>,
}

impl<'a> ProjectionPass<'a> {
    pub async fn new(
        client: &'a CdpClient,
        page_session: &'a str,
        iframe_sessions: &'a HashMap<String, String>,
    ) -> ProjectionResult<Self> {
        let topology = collect_frame_topology(client, page_session, iframe_sessions).await?;
        Ok(Self {
            client,
            page_session,
            iframe_sessions,
            topology,
            chains: HashMap::new(),
            viewports: HashMap::new(),
            embeddings: HashMap::new(),
            documents: HashMap::new(),
            handles: Vec::new(),
        })
    }

    pub fn include_frame(&mut self, frame: Option<&str>) -> ProjectionResult<String> {
        let id = frame.unwrap_or(&self.topology.top_frame_id).to_string();
        if !self.chains.contains_key(&id) {
            self.chains
                .insert(id.clone(), frame_chain(&self.topology, &id)?);
        }
        Ok(id)
    }

    pub async fn pin_selector(
        &mut self,
        refs: &RefMap,
        selector: &str,
        selected: Option<&FrameContext>,
    ) -> ProjectionResult<ResolvedTarget> {
        let frame = if let Some(id) = parse_ref(selector) {
            refs.get(&id)
                .ok_or_else(|| format!("Unknown ref: {id}"))?
                .frame_id
                .as_deref()
        } else {
            selected.map(|frame| frame.frame_id.as_str())
        };
        let frame_id = self.include_frame(frame)?;
        let (object, session) = resolve_element_object_id(
            self.client,
            self.page_session,
            refs,
            selector,
            self.iframe_sessions,
        )
        .await?;
        let handle = RemoteHandle { object, session };
        self.handles.push(handle.clone());
        if handle.session != self.chains[&frame_id][0].session_id {
            return Err(ProjectionError::Changed(
                "frame document or renderer".into(),
            ));
        }
        Ok(ResolvedTarget { frame_id, handle })
    }

    /// Register every acquired handle, including successes after a failed
    /// concurrent resolve, before propagating an error to the cleanup boundary.
    pub async fn resolve_refs(
        &mut self,
        refs: &RefMap,
    ) -> ProjectionResult<Vec<(String, RefEntry, ResolvedTarget)>> {
        let mut inputs = Vec::new();
        for (id, entry) in refs.entries_sorted() {
            let Some(backend) = entry.backend_node_id else {
                continue;
            };
            let frame_id = self.include_frame(entry.frame_id.as_deref())?;
            let session = entry
                .session_id
                .clone()
                .unwrap_or_else(|| self.chains[&frame_id][0].session_id.clone());
            if session != self.chains[&frame_id][0].session_id {
                return Err(ProjectionError::Changed(
                    "frame document or renderer".into(),
                ));
            }
            inputs.push((id, entry, frame_id, session, backend));
        }
        let client = self.client;
        let mut results = Vec::new();
        for chunk in inputs.chunks(MEASUREMENT_CONCURRENCY) {
            results.extend(
                join_all(
                    chunk
                        .iter()
                        .map(|(id, entry, frame_id, session, backend)| async move {
                            let value = client
                                .send_command(
                                    "DOM.resolveNode",
                                    Some(json!({"backendNodeId":backend})),
                                    Some(session),
                                )
                                .await?;
                            let object = value
                                .pointer("/object/objectId")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    unavailable("annotation target has no remote handle")
                                })?
                                .to_string();
                            Ok::<_, String>((
                                id.clone(),
                                entry.clone(),
                                ResolvedTarget {
                                    frame_id: frame_id.clone(),
                                    handle: RemoteHandle {
                                        session: session.clone(),
                                        object,
                                    },
                                },
                            ))
                        }),
                )
                .await,
            );
        }
        let mut targets = Vec::new();
        let mut error = None;
        for result in results {
            match result {
                Ok((id, entry, target)) => {
                    self.handles.push(target.handle.clone());
                    targets.push((id, entry, target));
                }
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        Ok(targets)
    }

    pub async fn prepare(&mut self, capture: bool) -> ProjectionResult<()> {
        let mut frames = HashMap::new();
        for chain in self.chains.values() {
            for frame in chain {
                frames.insert(frame.frame_id.clone(), frame.clone());
            }
        }
        let mut sessions: Vec<_> = frames.values().map(|f| f.session_id.clone()).collect();
        sessions.sort();
        sessions.dedup();
        for session in sessions {
            let metrics = self
                .client
                .send_command_no_params("Page.getLayoutMetrics", Some(&session))
                .await?;
            viewport_supported(&metrics)?;
            self.viewports.insert(session, metrics);
        }
        let mut ordered: Vec<_> = frames.values().cloned().collect();
        ordered.sort_by(|a, b| a.frame_id.cmp(&b.frame_id));
        for child in &ordered {
            if child.frame_id == self.topology.top_frame_id {
                continue;
            }
            let parent = frames
                .get(
                    child
                        .parent_id
                        .as_deref()
                        .ok_or_else(|| unavailable("missing parent frame"))?,
                )
                .ok_or_else(|| unavailable("missing parent renderer"))?;
            let owner = self
                .client
                .send_command(
                    "DOM.getFrameOwner",
                    Some(json!({"frameId":child.frame_id})),
                    Some(&parent.session_id),
                )
                .await?;
            let backend = owner["backendNodeId"]
                .as_i64()
                .ok_or_else(|| unavailable("iframe owner is unavailable"))?;
            let resolved = self
                .client
                .send_command(
                    "DOM.resolveNode",
                    Some(json!({"backendNodeId":backend})),
                    Some(&parent.session_id),
                )
                .await?;
            let object = resolved
                .pointer("/object/objectId")
                .and_then(Value::as_str)
                .ok_or_else(|| unavailable("iframe owner has no remote handle"))?
                .to_string();
            let handle = RemoteHandle {
                object,
                session: parent.session_id.clone(),
            };
            self.handles.push(handle.clone());
            let dimensions =
                element_metrics(self.client, &handle.session, &handle.object, true).await?;
            let model = box_model(self.client, &handle.session, &handle.object).await?;
            let mapping = AxisAlignedMap::from_box_model(&model, &dimensions)?;
            let content = Quad::read(&model["content"])?;
            self.embeddings.insert(
                child.frame_id.clone(),
                Embedding {
                    handle,
                    dimensions,
                    model,
                    mapping,
                    content,
                },
            );
        }
        if capture {
            for frame in ordered {
                let context_frame = FrameContext {
                    frame_id: frame.frame_id.clone(),
                    session_id: frame.session_id.clone(),
                };
                let context = frame_execution_context(
                    self.client,
                    self.page_session,
                    (frame.frame_id != self.topology.top_frame_id).then_some(&context_frame),
                )
                .await?;
                let value = evaluate_in_context(
                    self.client,
                    &context,
                    "document.documentElement",
                    false,
                    false,
                )
                .await?;
                if let Some(exception) = &value.exception_details {
                    release_exception_objects(
                        self.client,
                        &frame.session_id,
                        value.result.object_id.as_deref(),
                        exception
                            .exception
                            .as_ref()
                            .and_then(|e| e.object_id.as_deref()),
                    )
                    .await;
                    return Err(unavailable("document root could not be resolved").into());
                }
                let object = value
                    .result
                    .object_id
                    .ok_or_else(|| unavailable("document root is unavailable"))?;
                let handle = RemoteHandle {
                    object,
                    session: frame.session_id.clone(),
                };
                self.handles.push(handle.clone());
                let metrics = document_metrics(self.client, &handle).await?;
                self.documents
                    .insert(frame.frame_id, DocumentProbe { handle, metrics });
            }
        }
        Ok(())
    }

    fn project(&self, mut quad: Quad, chain: &[FrameTarget]) -> ProjectionResult<Quad> {
        for pair in chain.windows(2) {
            if pair[0].session_id != pair[1].session_id {
                quad = self.embeddings[&pair[0].frame_id].mapping.apply(quad)?;
            }
        }
        Ok(quad)
    }

    pub async fn measure(
        &self,
        target: &ResolvedTarget,
        omit_no_layout: bool,
    ) -> ProjectionResult<Measurement> {
        let metrics = element_metrics(
            self.client,
            &target.handle.session,
            &target.handle.object,
            false,
        )
        .await?;
        if omit_no_layout && metrics["noLayout"] == true {
            return Ok(Measurement::NoLayout);
        }
        let model = box_model(self.client, &target.handle.session, &target.handle.object).await?;
        let chain = &self.chains[&target.frame_id];
        let border = self.project(Quad::read(&model["border"])?, chain)?;
        if omit_no_layout && !border.bounds().positive() {
            return Ok(Measurement::NoLayout);
        }
        let mut embedding_clips = Vec::new();
        for (i, pair) in chain.windows(2).enumerate() {
            let owner = &self.embeddings[&pair[0].frame_id];
            embedding_clips.push(self.project(owner.content, &chain[i + 1..])?.bounds());
        }
        Ok(Measurement::Box(ProjectedGeometry {
            border,
            embedding_clips,
        }))
    }

    pub async fn measure_refs(
        &self,
        targets: &[(String, RefEntry, ResolvedTarget)],
    ) -> ProjectionResult<Vec<ProjectedRef>> {
        let mut results = Vec::new();
        for chunk in targets.chunks(MEASUREMENT_CONCURRENCY) {
            results.extend(
                join_all(chunk.iter().map(|(id, entry, target)| async move {
                    Ok::<_, ProjectionError>(match self.measure(target, true).await? {
                        Measurement::Box(geometry) => Some(ProjectedRef {
                            ref_id: id.clone(),
                            entry: entry.clone(),
                            geometry,
                        }),
                        Measurement::NoLayout => None,
                    })
                }))
                .await,
            );
        }
        Ok(results
            .into_iter()
            .collect::<ProjectionResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect())
    }

    pub fn page_metrics(&self) -> &Value {
        &self.viewports[self.page_session]
    }
    pub fn page_scroll(&self) -> &Value {
        &self.documents[&self.topology.top_frame_id].metrics
    }
    pub fn page_frame(&self) -> FrameContext {
        FrameContext {
            frame_id: self.topology.top_frame_id.clone(),
            session_id: self.page_session.to_string(),
        }
    }

    pub fn document_identity(&self, frame_id: &str) -> Value {
        let frame = &self.topology.frames[frame_id];
        json!({"frame":frame.frame_id,"loader":frame.loader_id,"session":frame.session_id})
    }

    /// Stable identity only: moving boxes and scroll offsets must not create new
    /// conditional screenshot scopes and bypass normal pixel comparison.
    pub fn document_signature(&self) -> Value {
        let mut frames: Vec<_> = self.chains.values().flatten().collect();
        frames.sort_by(|a, b| a.frame_id.cmp(&b.frame_id));
        frames.dedup_by(|a, b| a.frame_id == b.frame_id);
        json!(frames
            .into_iter()
            .map(|f| self.document_identity(&f.frame_id))
            .collect::<Vec<_>>())
    }

    pub async fn validate(&self, capture: bool) -> ProjectionResult<()> {
        let current =
            collect_frame_topology(self.client, self.page_session, self.iframe_sessions).await?;
        if current.top_frame_id != self.topology.top_frame_id {
            return Err(ProjectionError::Changed(
                "frame document or renderer".into(),
            ));
        }
        for (id, chain) in &self.chains {
            if frame_chain(&current, id)? != *chain {
                return Err(ProjectionError::Changed(
                    "frame document or renderer".into(),
                ));
            }
        }
        if capture {
            for (session, before) in &self.viewports {
                let after = self
                    .client
                    .send_command_no_params("Page.getLayoutMetrics", Some(session))
                    .await?;
                viewport_supported(&after)?;
                // OOPIF visual-viewport dimensions mirror the top page and can
                // lag its emulation resize. They are not the child viewport.
                // Validate scale/origin there, but use each document's own
                // scroll/size probe and its owner's quad for child stability.
                if session == self.page_session {
                    for key in ["cssVisualViewport", "cssLayoutViewport", "cssContentSize"] {
                        if !metrics_match(&before[key], &after[key]) {
                            return Err(ProjectionError::Changed(format!("top-page {key}")));
                        }
                    }
                }
            }
            for document in self.documents.values() {
                if !metrics_match(
                    &document.metrics,
                    &document_metrics(self.client, &document.handle).await?,
                ) {
                    return Err(ProjectionError::Changed(
                        "document scrolling or viewport size".into(),
                    ));
                }
            }
            for embedding in self.embeddings.values() {
                let handle = &embedding.handle;
                let dimensions =
                    element_metrics(self.client, &handle.session, &handle.object, true).await?;
                let model = box_model(self.client, &handle.session, &handle.object).await?;
                AxisAlignedMap::from_box_model(&model, &dimensions)?;
                if !metrics_match(&dimensions, &embedding.dimensions)
                    || !metrics_match(&model["content"], &embedding.model["content"])
                    || (dimensions["boxSizing"] == "border-box"
                        && !metrics_match(&model["border"], &embedding.model["border"]))
                {
                    return Err(ProjectionError::Changed(
                        "iframe sizing or content mapping".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub async fn release(&mut self) {
        let client = self.client;
        let handles = std::mem::take(&mut self.handles);
        for chunk in handles.chunks(MEASUREMENT_CONCURRENCY) {
            join_all(chunk.iter().map(|handle| async move {
                let _ = client
                    .send_command(
                        "Runtime.releaseObject",
                        Some(json!({"objectId":handle.object})),
                        Some(&handle.session),
                    )
                    .await;
            }))
            .await;
        }
    }
}

/// Single-target adapter. Preserve get-box's unclipped border bounds, normal
/// errors, and one read-only attempt without adopting annotation/capture policy.
pub(super) async fn top_viewport_box(
    client: &CdpClient,
    page_session: &str,
    refs: &RefMap,
    selector: &str,
    selected: Option<&FrameContext>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Value, String> {
    let mut pass = ProjectionPass::new(client, page_session, iframe_sessions)
        .await
        .map_err(|e| e.to_string())?;
    let result: ProjectionResult<Value> = async {
        let target = pass.pin_selector(refs, selector, selected).await?;
        pass.prepare(false).await?;
        let Measurement::Box(geometry) = pass.measure(&target, false).await? else {
            unreachable!()
        };
        pass.validate(false).await?;
        serde_json::to_value(geometry.bounds()).map_err(|e| unavailable(&e.to_string()).into())
    }
    .await;
    pass.release().await;
    result.map_err(|e| e.to_string())
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
        let inner = AxisAlignedMap::from_quad(
            quad([35., 45., 735., 45., 735., 545., 35., 545.]),
            350.,
            250.,
        )
        .unwrap();
        let outer = AxisAlignedMap::from_quad(
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
        let fractional = AxisAlignedMap::from_quad(
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
            assert!(AxisAlignedMap::from_quad(quad(values), 100., 100.).is_err());
        }
        for width in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(AxisAlignedMap::from_quad(
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
                            "inner-owner" => {
                                json!({"width":350,"height":250,"boxSizing":"content-box"})
                            }
                            "outer-owner" => {
                                json!({"width":520,"height":420,"boxSizing":"border-box"})
                            }
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
                            json!({"model":{"content":[250,170,750,170,750,570,250,570],"border":[240,160,760,160,760,580,240,580]}})
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

#[cfg(test)]
mod batch_tests;
