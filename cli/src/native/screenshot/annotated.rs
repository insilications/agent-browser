//! Chrome annotations consume renderer-aware geometry; capture origin and
//! rectangular drawing clips are presentation policy, never get-box policy.
use std::collections::HashMap;

use futures_util::{future::BoxFuture, FutureExt};
use serde::Serialize;
use serde_json::{json, Value};

use super::{AnnotationBox, ScreenshotAnnotation, ScreenshotOptions, ScreenshotResult};
use crate::native::cdp::{client::CdpClient, types::*};
use crate::native::element::{evaluate_in_context, frame_execution_context, FrameContext, RefMap};
use crate::native::geometry::{
    release_exception_objects, Measurement, ProjectedRef, ProjectionPass, Rect, EPSILON,
};
use crate::native::snapshot::{take_snapshot, SnapshotOptions};

#[derive(Debug, Clone, Copy, PartialEq)]
enum CaptureMode {
    Viewport,
    FullPage,
    Element,
}

/// One immutable coordinate conversion shared by CDP capture, overlay drawing,
/// filtering and response serialization. All dimensions remain CSS pixels.
struct CapturePlan {
    mode: CaptureMode,
    document: Rect,
    viewport: Rect,
    selector: Option<Rect>,
    scroll_x: f64,
    scroll_y: f64,
}

fn metric(value: &Value) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| "Required CSS capture metrics are unavailable".to_string())
}

impl CapturePlan {
    fn new(
        options: &ScreenshotOptions,
        metrics: &Value,
        scroll: &Value,
        selector: Option<Rect>,
    ) -> Result<Self, String> {
        let scroll_x = metric(&scroll["x"])?;
        let scroll_y = metric(&scroll["y"])?;
        let content = Rect::read(&metrics["cssContentSize"])?;
        let visible = Rect {
            x: scroll_x,
            y: scroll_y,
            width: metric(&metrics["cssVisualViewport"]["clientWidth"])?,
            height: metric(&metrics["cssVisualViewport"]["clientHeight"])?,
        };
        if !content.positive() || !visible.positive() {
            return Err("Page has no positive CSS capture area".into());
        }
        if selector.is_some_and(|rect| !rect.positive()) {
            return Err("Screenshot selector has no measurable positive box".into());
        }
        let (mode, document) = if options.full_page {
            (CaptureMode::FullPage, content)
        } else if let Some(rect) = selector {
            (
                CaptureMode::Element,
                rect.translated(scroll_x, scroll_y)
                    .intersection(content)
                    .ok_or("Screenshot selector is outside the page's available content area")?,
            )
        } else {
            (CaptureMode::Viewport, visible)
        };
        Ok(Self {
            mode,
            document,
            viewport: document.translated(-scroll_x, -scroll_y),
            selector,
            scroll_x,
            scroll_y,
        })
    }

    fn relative(&self, rect: Rect) -> Rect {
        rect.translated(
            self.scroll_x - self.document.x,
            self.scroll_y - self.document.y,
        )
    }

    fn params(&self, options: &ScreenshotOptions) -> CaptureScreenshotParams {
        CaptureScreenshotParams {
            format: Some(options.format.clone()),
            quality: (options.format == "jpeg").then_some(options.quality.unwrap_or(80)),
            // Default viewport capture retains Chrome's native viewport surface.
            clip: (self.mode != CaptureMode::Viewport).then_some(Viewport {
                x: self.document.x,
                y: self.document.y,
                width: self.document.width,
                height: self.document.height,
                scale: 1.0,
            }),
            from_surface: Some(true),
            capture_beyond_viewport: Some(self.mode != CaptureMode::Viewport),
        }
    }

    fn drawing_clip(&self, rect: Rect, embeddings: &[Rect]) -> Option<Rect> {
        if self
            .selector
            .is_some_and(|target| rect.intersection(target).is_none())
        {
            return None;
        }
        embeddings
            .iter()
            .try_fold(self.viewport, |clip, frame| clip.intersection(*frame))
            .filter(|clip| rect.intersection(*clip).is_some())
    }
}

#[derive(Serialize)]
struct Drawing {
    number: u64,
    rect: Rect,
    clip: Rect,
}

fn annotations(
    plan: &CapturePlan,
    candidates: Vec<ProjectedRef>,
) -> (Vec<Drawing>, Vec<ScreenshotAnnotation>) {
    let mut drawings = Vec::new();
    let mut annotations = Vec::new();
    for candidate in candidates {
        let rect = candidate.geometry.bounds();
        let Some(clip) = plan.drawing_clip(rect, &candidate.geometry.embedding_clips) else {
            continue;
        };
        let number = candidate
            .ref_id
            .strip_prefix('e')
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let relative = plan.relative(rect);
        drawings.push(Drawing { number, rect, clip });
        annotations.push(ScreenshotAnnotation {
            ref_id: candidate.ref_id,
            number,
            role: candidate.entry.role,
            name: (!candidate.entry.name.is_empty()).then_some(candidate.entry.name),
            box_: AnnotationBox {
                x: relative.x,
                y: relative.y,
                width: relative.width,
                height: relative.height,
            },
        });
    }
    (drawings, annotations)
}

pub(crate) struct AnnotatedCapture {
    pub result: ScreenshotResult,
    pub document_signature: Value,
}

/// Pin a crop target before refreshing refs. Everything acquired by this call
/// is released even if snapshot, batch acquisition, rendering or capture fails.
pub(crate) fn take_annotated_screenshot<'a>(
    client: &'a CdpClient,
    page_session: &'a str,
    refs: &'a mut RefMap,
    options: &'a ScreenshotOptions,
    selected: Option<&'a FrameContext>,
    iframe_sessions: &'a HashMap<String, String>,
) -> BoxFuture<'a, Result<AnnotatedCapture, String>> {
    // Keep the already-large daemon dispatch future and its Send proof bounded.
    async move {
        let mut pass = ProjectionPass::new(client, page_session, iframe_sessions)
            .await
            .map_err(|e| e.to_string())?;
        let result: Result<AnnotatedCapture, String> = async {
            let snapshot_frame = pass
                .include_frame(selected.map(|frame| frame.frame_id.as_str()))
                .map_err(|e| e.to_string())?;
            let crop = match options.selector.as_deref() {
                Some(selector) => Some(
                    pass.pin_selector(refs, selector, selected)
                        .await
                        .map_err(|e| e.to_string())?,
                ),
                None => None,
            };
            refs.begin_snapshot();
            take_snapshot(
                client,
                page_session,
                &SnapshotOptions {
                    interactive: true,
                    ..SnapshotOptions::default()
                },
                refs,
                selected.map(|f| f.frame_id.as_str()),
                selected.map(|f| f.session_id.as_str()),
                iframe_sessions,
            )
            .await?;
            let targets = pass.resolve_refs(refs).await.map_err(|e| e.to_string())?;
            pass.prepare(true).await.map_err(|e| e.to_string())?;
            let selector = match crop {
                Some(ref target) => match pass
                    .measure(target, true)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Measurement::Box(geometry) => Some(geometry.bounds()),
                    Measurement::NoLayout => {
                        return Err("Screenshot selector has no measurable positive box".into())
                    }
                },
                None => None,
            };
            let candidates = pass
                .measure_refs(&targets)
                .await
                .map_err(|e| e.to_string())?;
            let plan =
                CapturePlan::new(options, pass.page_metrics(), pass.page_scroll(), selector)?;
            let (drawings, annotations) = annotations(&plan, candidates);
            // Reject changed ownership before touching the DOM. The complete geometry
            // validation follows capture and cleanup; this is not stability polling.
            pass.validate(false).await.map_err(|e| e.to_string())?;
            let base64 =
                capture_with_overlay(client, &pass.page_frame(), &plan, &drawings, options).await?;
            pass.validate(true).await.map_err(|e| e.to_string())?;
            Ok(AnnotatedCapture {
                result: ScreenshotResult {
                    base64,
                    annotations,
                },
                // Distinguish discovery/crop roles even when their union of
                // participating documents is identical across two scopes.
                document_signature: json!({
                    "snapshot":pass.document_identity(&snapshot_frame),
                    "selector":crop.as_ref().map(|target| pass.document_identity(&target.frame_id)),
                    "participants":pass.document_signature(),
                }),
            })
        }
        .await;
        pass.release().await;
        result.map_err(|error| format!("Cannot capture annotated screenshot: {error}"))
    }
    .boxed()
}

/// Finish cleanup before returning bytes to the validation/save boundary.
async fn capture_with_overlay(
    client: &CdpClient,
    page: &FrameContext,
    plan: &CapturePlan,
    drawings: &[Drawing],
    options: &ScreenshotOptions,
) -> Result<String, String> {
    let overlay = if drawings.is_empty() {
        None
    } else {
        Some(insert_overlay(client, page, plan, drawings).await?)
    };
    let capture = async {
        let capture: CaptureScreenshotResult = client
            .send_command_typed(
                "Page.captureScreenshot",
                &plan.params(options),
                Some(&page.session_id),
            )
            .await?;
        if let Some(handle) = &overlay {
            overlay_call(client, &page.session_id, handle, "check").await?;
        }
        Ok::<_, String>(capture.data)
    }
    .await;
    let cleanup = if let Some(handle) = &overlay {
        let result = overlay_call(client, &page.session_id, handle, "remove").await;
        let _ = client
            .send_command(
                "Runtime.releaseObject",
                Some(json!({"objectId":handle})),
                Some(&page.session_id),
            )
            .await;
        result
    } else {
        Ok(())
    };
    match (capture, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; overlay cleanup: {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(base64), Ok(())) => Ok(base64),
    }
}

async fn overlay_call(
    client: &CdpClient,
    session: &str,
    object: &str,
    operation: &str,
) -> Result<(), String> {
    let value = client.send_command("Runtime.callFunctionOn",Some(json!({
        "objectId":object, "functionDeclaration":format!("function() {{ return this.{operation}(); }}"),
        "returnByValue":true,
    })),Some(session)).await?;
    if value.get("exceptionDetails").is_some()
        || value.pointer("/result/value") != Some(&Value::Bool(true))
    {
        release_exception_objects(
            client,
            session,
            value.pointer("/result/objectId").and_then(Value::as_str),
            value
                .pointer("/exceptionDetails/exception/objectId")
                .and_then(Value::as_str),
        )
        .await;
        let detail = value
            .pointer("/exceptionDetails/exception/description")
            .and_then(Value::as_str)
            .unwrap_or("overlay operation did not complete");
        return Err(format!("Annotation overlay {operation} failed: {detail}"));
    }
    Ok(())
}

async fn insert_overlay(
    client: &CdpClient,
    page: &FrameContext,
    plan: &CapturePlan,
    drawings: &[Drawing],
) -> Result<String, String> {
    let context = frame_execution_context(client, &page.session_id, Some(page)).await?;
    let data = json!({"origin":plan.viewport,"items":drawings,"epsilon":EPSILON,"id":format!("__agent_browser_annotations_{}",uuid::Uuid::new_v4())});
    let expression = format!("({OVERLAY_JS})({data})");
    let result = evaluate_in_context(client, &context, &expression, false, false).await?;
    if let Some(exception) = result.exception_details {
        release_exception_objects(
            client,
            &page.session_id,
            result.result.object_id.as_deref(),
            exception
                .exception
                .as_ref()
                .and_then(|e| e.object_id.as_deref()),
        )
        .await;
        return Err(format!(
            "Unsupported annotation overlay basis or insertion failure: {}",
            exception
                .exception
                .and_then(|e| e.description)
                .unwrap_or(exception.text)
        ));
    }
    let handle = result
        .result
        .object_id
        .ok_or("Annotation overlay has no cleanup handle")?;
    // The controller is retained before its host is mounted. A failed or lost
    // mount response therefore still leaves an exact cleanup handle.
    if let Err(error) = overlay_call(client, &page.session_id, &handle, "mount").await {
        let cleanup = overlay_call(client, &page.session_id, &handle, "remove").await;
        let _ = client
            .send_command(
                "Runtime.releaseObject",
                Some(json!({"objectId":handle})),
                Some(&page.session_id),
            )
            .await;
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; overlay cleanup: {cleanup}"),
        });
    }
    Ok(handle)
}

// Run in an isolated world. Shadow DOM isolates label styles, while a measured
// unit basis prevents document-root transforms from applying projection twice.
// Host translation is calibrated from browser geometry, not CSS composition.
const OVERLAY_JS: &str = r#"(data) => {
    const near = (a,b) => Number.isFinite(a) && Math.abs(a-b) <= data.epsilon;
    const guard = () => {
        const s = getComputedStyle(document.documentElement);
        if ((s.zoom !== 'normal' && Number(s.zoom) !== 1) || s.perspective !== 'none' ||
            s.transformStyle !== 'flat' || s.offsetPath !== 'none') throw Error('unsupported root zoom, perspective or motion path');
        if (s.transform !== 'none') {
            const m = new DOMMatrixReadOnly(s.transform);
            if (!m.is2D || !near(m.a,1) || !near(m.d,1) || !near(m.b,0) || !near(m.c,0)) throw Error('root must preserve the annotation unit coordinate basis');
        }
        if (s.rotate !== 'none' && !/^0(?:deg|rad|grad|turn)?$/.test(s.rotate)) throw Error('root rotation is unsupported for annotations');
        if (s.scale !== 'none' && s.scale.split(/\s+/).some(n => Number(n) !== 1)) throw Error('root scaling is unsupported for annotations');
        if (s.translate !== 'none') {
            const p=s.translate.split(/\s+/);
            if (p.length > 2 && !/^0(?:px)?$/.test(p[2])) throw Error('root 3D translation is unsupported for annotations');
        }
    };
    guard();
    const host = document.createElement('div');
    host.id = data.id;
    host.setAttribute('aria-hidden','true');
    const set = (name,value) => host.style.setProperty(name,value,'important');
    set('all','initial'); set('position','absolute'); set('left','0'); set('top','0');
    set('width','0'); set('height','0'); set('margin','0'); set('padding','0'); set('border','0');
    set('box-sizing','border-box'); set('overflow','hidden'); set('contain','strict');
    set('pointer-events','none'); set('z-index','2147483647'); set('visibility','visible');
    set('display','block'); set('opacity','1'); set('transform-origin','0 0');
    set('transform','none'); set('translate','none'); set('rotate','none'); set('scale','none');
    set('transition','none'); set('animation','none');
    const root = host.attachShadow({mode:'closed'});
    const node = (css,parent=root) => {
        const el=document.createElement('div'); el.style.cssText='all:initial;position:absolute;box-sizing:border-box;pointer-events:none;'+css;
        parent.appendChild(el); return el;
    };
    const probes = [[0,0],[1,0],[0,1]].map(([x,y]) => node(`left:${x}px;top:${y}px;width:1px;height:1px;visibility:hidden;`));
    const basis = () => {
        const [a,b,c]=probes.map(p=>p.getBoundingClientRect());
        if (!near(b.x-a.x,1) || !near(b.y-a.y,0) || !near(c.x-a.x,0) || !near(c.y-a.y,1) ||
            !near(a.width,1) || !near(a.height,1)) throw Error('annotation coordinate basis is not unit and axis-aligned');
        return a;
    };
    const check = () => {
        if (!host.isConnected) throw Error('annotation overlay was detached');
        guard(); const a=basis();
        if (!near(a.x,data.origin.x) || !near(a.y,data.origin.y)) throw Error('annotation overlay origin changed');
        return true;
    };
    const mount = () => { try {
        document.documentElement.appendChild(host);
        const before=basis();
        set('width',data.origin.width+'px'); set('height',data.origin.height+'px');
        set('transform',`translate(${data.origin.x-before.x}px, ${data.origin.y-before.y}px)`);
        check();
        for (const item of data.items) {
            const r=item.rect, c=item.clip;
            const clip=node(`left:${c.x-data.origin.x}px;top:${c.y-data.origin.y}px;width:${c.width}px;height:${c.height}px;overflow:hidden;`);
            node(`left:${r.x-c.x}px;top:${r.y-c.y}px;width:${r.width}px;height:${r.height}px;border:2px solid rgba(255,0,0,0.8);`,clip);
            const label=node('background:rgba(255,0,0,0.9);color:#fff;font:bold 11px/14px monospace;padding:0 4px;border-radius:2px;white-space:nowrap;',clip);
            label.textContent=String(item.number);
            const size=label.getBoundingClientRect();
            const x=Math.max(0,Math.min(r.x-c.x,c.width-size.width));
            const preferred=r.y-c.y>=14 ? r.y-c.y-14 : Math.max(0,r.y-c.y)+2;
            const y=Math.max(0,Math.min(preferred,c.height-size.height));
            label.style.left=x+'px'; label.style.top=y+'px';
        }
        return true;
    } catch (error) {
        host.remove(); throw error;
    }};
    return {mount,check,remove:()=>{host.remove();return !host.isConnected;}};
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn plan(full: bool, selector: Option<Rect>) -> CapturePlan {
        CapturePlan::new(
            &ScreenshotOptions {
                full_page: full,
                ..Default::default()
            },
            &json!({"cssContentSize":{"x":0,"y":0,"width":800,"height":2000},
                "cssVisualViewport":{"clientWidth":800,"clientHeight":600}}),
            &json!({"x":0.25,"y":200.5}),
            selector,
        )
        .unwrap()
    }

    #[test]
    fn capture_origins_preserve_fractional_css_boxes_and_full_precedence() {
        let target = rect(120.125, 299.75, 80.5, 40.25);
        let viewport = plan(false, None);
        assert_eq!(viewport.relative(target), target);
        assert!(viewport
            .params(&ScreenshotOptions::default())
            .clip
            .is_none());
        let full = plan(true, Some(target));
        assert_eq!(full.mode, CaptureMode::FullPage);
        assert_eq!(full.relative(target), rect(120.375, 500.25, 80.5, 40.25));
        assert_eq!(full.document, rect(0., 0., 800., 2000.));
        let crop = plan(false, Some(target));
        assert_eq!(crop.relative(target), rect(0., 0., 80.5, 40.25));
        let params = crop.params(&ScreenshotOptions::default());
        let clip = params.clip.unwrap();
        assert_eq!(
            (clip.x, clip.y, clip.width, clip.height),
            (120.375, 500.25, 80.5, 40.25)
        );
        assert_eq!(params.capture_beyond_viewport, Some(true));
        assert_eq!(clip.scale, 1.);
    }

    #[test]
    fn crop_content_intersection_does_not_shrink_returned_geometry() {
        let target = rect(-10.75, -205.25, 80.5, 40.25);
        let crop = plan(false, Some(target));
        assert_eq!(crop.document, rect(0., 0., 70., 35.5));
        assert_eq!(crop.relative(target), rect(-10.5, -4.75, 80.5, 40.25));
        let metrics = json!({"cssContentSize":{"x":0,"y":0,"width":800,"height":600},
            "cssVisualViewport":{"clientWidth":800,"clientHeight":600}});
        for target in [rect(-100., 0., 1., 1.), rect(0., 0., 0., 2.)] {
            assert!(CapturePlan::new(
                &ScreenshotOptions::default(),
                &metrics,
                &json!({"x":0,"y":0}),
                Some(target)
            )
            .is_err());
        }
        assert!(CapturePlan::new(
            &ScreenshotOptions::default(),
            &json!({"contentSize":{"x":0,"y":0,"width":800,"height":600}}),
            &json!({"x":0,"y":0}),
            None
        )
        .is_err());
    }

    #[test]
    fn nested_drawing_clips_are_separate_from_full_geometric_bounds() {
        let viewport = plan(false, None);
        let target = rect(-10., 50., 80., 40.);
        let clips = [rect(0., 20., 60., 100.), rect(20., 40., 80., 20.)];
        assert_eq!(
            viewport.drawing_clip(target, &clips),
            Some(rect(20., 40., 40., 20.))
        );
        assert_eq!(viewport.relative(target), target);
        assert!(viewport
            .drawing_clip(rect(60., 60., 80., 40.), &clips)
            .is_none());
        assert!(viewport
            .drawing_clip(target, &[rect(900., 0., 100., 100.)])
            .is_none());
        let full = plan(true, Some(rect(0., 0., 5., 5.)));
        assert!(full.drawing_clip(target, &[]).is_none());
    }
}

#[cfg(test)]
#[path = "annotated_tests.rs"]
mod lifecycle_tests;
