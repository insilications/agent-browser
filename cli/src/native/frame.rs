//! Keep a selected browsing frame usable when Chrome changes its renderer.
//!
//! The frame ID identifies the browsing context; its CDP session can change
//! when navigation moves it into or out of an OOPIF. Lifecycle events mark
//! ownership as pending, then command preparation resolves that same ID in
//! the active tab's frame trees. Removal stays invalid until explicit selection
//! or a reset: falling back to main could send iframe input to an unrelated page.
//!
//! Document and renderer replacement invalidate the affected element refs.
//! Backend node IDs are never transferred to the new session. Recovery happens
//! before dispatch, so it does not replay actions or rebind an in-flight wait.
use super::cdp::client::CdpClient;
use super::cdp::types::CdpEvent;
use super::element::{FrameContext, RefMap};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub(super) const FRAME_GONE: &str =
    "Selected frame is no longer available. Run `frame main` or select the frame again.";
pub(super) const FRAME_NOT_READY: &str =
    "Selected frame is not ready. Retry the command or run `frame main`.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SelectionStatus {
    /// The owning session and a default page context have been resolved.
    /// Application content may still be loading.
    Ready,
    /// A renderer swap or missing context needs another ownership check.
    #[default]
    Pending,
    /// The browsing frame was removed; matching HTML attributes cannot revive it.
    Removed,
}

/// The selected ID survives swaps; removal is sticky until explicit selection.
/// The last topology retains ancestry even after Chrome removes the subtree.
#[derive(Debug, Default)]
pub(super) struct SelectedFrameState {
    pub status: SelectionStatus,
    pub topology: Option<FrameTopology>,
    /// Invalidates an absence observation if more lifecycle events arrive.
    pub revision: u64,
}

impl SelectedFrameState {
    fn below(&self, frame_id: &str, ancestor: &str) -> bool {
        frame_id == ancestor
            || self
                .topology
                .as_ref()
                .is_some_and(|t| t.contains_descendant(frame_id, ancestor))
    }

    fn owns_event(&self, selected: &FrameContext, frame_id: &str, session: &str) -> bool {
        if selected.frame_id == frame_id && selected.session_id == session {
            return true;
        }
        let Some(topology) = &self.topology else {
            return false;
        };
        let Some(frame) = topology.frames.get(frame_id) else {
            return false;
        };
        // A remote iframe's removal is also reported by its parent renderer.
        let owner = if selected.frame_id == frame_id {
            &selected.session_id
        } else {
            &frame.session_id
        };
        owner == session
            || frame
                .parent_id
                .as_ref()
                .and_then(|id| topology.frames.get(id))
                .is_some_and(|parent| parent.session_id == session)
    }

    /// Drop refs in the departed document and its descendants. An iframe node
    /// belongs to its parent document, so a surviving parent-side ref can still
    /// be used for an explicit `frame @ref` selection.
    pub fn invalidate_document(&self, refs: &mut RefMap, frame_id: &str) {
        for (id, entry) in refs.entries_sorted() {
            if entry
                .frame_id
                .as_deref()
                .is_some_and(|id| self.below(id, frame_id))
            {
                refs.remove(&id);
            }
        }
    }

    /// Consume selection events before diagnostic filtering, in wire order.
    /// Events from other tabs cannot match this selection's recorded ancestry.
    pub fn apply_event(
        &mut self,
        selected: &mut FrameContext,
        refs: &mut RefMap,
        event: &CdpEvent,
    ) {
        self.revision = self.revision.wrapping_add(1);
        let params = &event.params;
        let session = event.session_id.as_deref().unwrap_or_default();
        match event.method.as_str() {
            "Target.attachedToTarget" => {
                let info = &params["targetInfo"];
                if info["type"] == "iframe" && info["targetId"] == selected.frame_id {
                    if let Some(sid) = params["sessionId"].as_str() {
                        if self.status != SelectionStatus::Removed && selected.session_id != sid {
                            self.invalidate_document(refs, &selected.frame_id);
                            selected.session_id = sid.to_string();
                            self.status = SelectionStatus::Pending;
                        }
                    }
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = params["sessionId"].as_str() {
                    for (id, entry) in refs.entries_sorted() {
                        if entry.session_id.as_deref() == Some(sid) {
                            refs.remove(&id);
                        }
                    }
                    if self.status != SelectionStatus::Removed && selected.session_id == sid {
                        self.status = SelectionStatus::Pending;
                    }
                }
            }
            "Page.frameDetached" => {
                let Some(id) = params["frameId"].as_str() else {
                    return;
                };
                if !self.owns_event(selected, id, session) {
                    return;
                }
                self.invalidate_document(refs, id);
                if self.below(&selected.frame_id, id) && self.status != SelectionStatus::Removed {
                    self.status = if params["reason"] == "remove" {
                        SelectionStatus::Removed
                    } else {
                        SelectionStatus::Pending
                    };
                }
            }
            "Page.frameNavigated" => {
                let Some(id) = params["frame"]["id"].as_str() else {
                    return;
                };
                if !self.owns_event(selected, id, session) {
                    return;
                }
                let loader = params["frame"]["loaderId"].as_str();
                let previous_loader = self
                    .topology
                    .as_ref()
                    .and_then(|t| t.frames.get(id))
                    .and_then(|f| f.loader_id.as_deref());
                if loader.is_some() && loader != previous_loader {
                    self.invalidate_document(refs, id);
                    if self.below(&selected.frame_id, id) && self.status != SelectionStatus::Removed
                    {
                        self.status = SelectionStatus::Pending;
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FrameTopology {
    pub top_frame_id: String,
    pub frames: HashMap<String, FrameTarget>,
    /// A failed target query is uncertainty, not evidence of frame removal.
    pub incomplete: bool,
}

impl FrameTopology {
    pub fn contains_descendant(&self, frame_id: &str, ancestor: &str) -> bool {
        frame_reaches_top(frame_id, ancestor, &self.frames)
    }
}

pub(super) async fn collect_frame_sessions(
    client: &CdpClient,
    top_session_id: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(String, Vec<FrameTarget>), String> {
    let topology = collect_frame_topology(client, top_session_id, iframe_sessions).await?;
    let mut frames: Vec<_> = topology.frames.into_values().collect();
    frames.sort_unstable_by(|a, b| a.frame_id.cmp(&b.frame_id));
    Ok((topology.top_frame_id, frames))
}

#[derive(Debug, Clone)]
pub(super) struct FrameTarget {
    pub frame_id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
    pub loader_id: Option<String>,
}

pub(super) fn collect_frame_targets(
    tree: &Value,
    query_session_id: &str,
    iframe_sessions: &HashMap<String, String>,
    targets: &mut HashMap<String, FrameTarget>,
) {
    collect_frame_targets_in_session(
        tree,
        query_session_id,
        query_session_id,
        iframe_sessions,
        targets,
    );
}

fn collect_frame_targets_in_session(
    tree: &Value,
    query_session_id: &str,
    parent_session_id: &str,
    iframe_sessions: &HashMap<String, String>,
    targets: &mut HashMap<String, FrameTarget>,
) {
    let session_id = if let Some(frame) = tree.get("frame") {
        let Some(frame_id) = frame.get("id").and_then(|id| id.as_str()) else {
            return;
        };
        // Same-process child frames do not have their own target session. They
        // execute in the nearest ancestor target, which may itself be an
        // out-of-process iframe rather than the top-level page.
        let session_id = iframe_sessions
            .get(frame_id)
            .cloned()
            .unwrap_or_else(|| parent_session_id.to_string());
        let target = FrameTarget {
            loader_id: frame
                .get("loaderId")
                .and_then(Value::as_str)
                .map(str::to_string),
            frame_id: frame_id.to_string(),
            session_id: session_id.clone(),
            parent_id: frame
                .get("parentId")
                .and_then(|id| id.as_str())
                .map(ToString::to_string),
        };
        targets
            .entry(frame_id.to_string())
            .and_modify(|existing| {
                // Prefer the tree from the renderer that owns this document,
                // including same-process children without dedicated targets.
                // Keep the actual query session separate from the inherited
                // owner: traversing a remote placeholder does not query it.
                if session_id == query_session_id {
                    let mut authoritative = target.clone();
                    if authoritative.parent_id.is_none() {
                        authoritative.parent_id.clone_from(&existing.parent_id);
                    }
                    *existing = authoritative;
                } else if existing.parent_id.is_none() {
                    // A dedicated target root can omit its parent. Merge the
                    // containing renderer's link even if that tree comes later.
                    existing.parent_id.clone_from(&target.parent_id);
                }
            })
            .or_insert(target);
        session_id
    } else {
        parent_session_id.to_string()
    };
    if let Some(children) = tree.get("childFrames").and_then(|value| value.as_array()) {
        for child in children {
            collect_frame_targets_in_session(
                child,
                query_session_id,
                &session_id,
                iframe_sessions,
                targets,
            );
        }
    }
}

pub(super) fn frame_reaches_top(
    frame_id: &str,
    top_frame_id: &str,
    targets: &HashMap<String, FrameTarget>,
) -> bool {
    let mut current = frame_id;
    let mut visited = HashSet::new();
    loop {
        if current == top_frame_id {
            return true;
        }
        if !visited.insert(current.to_string()) {
            return false;
        }
        let Some(parent_id) = targets
            .get(current)
            .and_then(|target| target.parent_id.as_deref())
        else {
            return false;
        };
        current = parent_id;
    }
}

pub(super) async fn collect_frame_topology(
    client: &CdpClient,
    top_session_id: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<FrameTopology, String> {
    let top_tree = client
        .send_command_no_params("Page.getFrameTree", Some(top_session_id))
        .await?;
    let top_frame_id = top_tree
        .get("frameTree")
        .and_then(|tree| tree.get("frame"))
        .and_then(|frame| frame.get("id"))
        .and_then(|id| id.as_str())
        .ok_or("Could not determine top-level frame ID")?
        .to_string();

    let mut targets = HashMap::new();
    let mut incomplete = false;
    if let Some(tree) = top_tree.get("frameTree") {
        collect_frame_targets(tree, top_session_id, iframe_sessions, &mut targets);
    }

    // Query every attached iframe target. The top target's frame tree can
    // omit descendants below an OOPIF, while the OOPIF's own tree exposes
    // those same-process descendants with the correct execution session.
    let mut session_entries: Vec<_> = iframe_sessions.values().collect();
    session_entries.sort_unstable();
    session_entries.dedup();
    for session_id in session_entries {
        let Some(tree) = client
            .send_command_no_params("Page.getFrameTree", Some(session_id))
            .await
            .ok()
            .and_then(|result| result.get("frameTree").cloned())
        else {
            incomplete = true;
            continue;
        };
        collect_frame_targets(&tree, session_id, iframe_sessions, &mut targets);
    }

    // The daemon retains sessions for background tabs. Keep only frames whose
    // parent chain reaches the active page. Query order must not decide whether
    // a nested OOPIF belongs to this tab.
    let all_targets = targets.clone();
    targets.retain(|_, target| frame_reaches_top(&target.frame_id, &top_frame_id, &all_targets));
    Ok(FrameTopology {
        top_frame_id,
        frames: targets,
        incomplete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (SelectedFrameState, FrameContext, RefMap) {
        let mut frames = HashMap::new();
        for (id, sid, parent) in [
            ("top", "page", None),
            ("outer", "oopif", Some("top")),
            ("inner", "oopif", Some("outer")),
        ] {
            frames.insert(
                id.to_string(),
                FrameTarget {
                    frame_id: id.into(),
                    session_id: sid.into(),
                    parent_id: parent.map(str::to_string),
                    loader_id: Some("old-document".into()),
                },
            );
        }
        let lifecycle = SelectedFrameState {
            status: SelectionStatus::Ready,
            topology: Some(FrameTopology {
                top_frame_id: "top".into(),
                frames,
                incomplete: false,
            }),
            revision: 0,
        };
        let selected = FrameContext {
            frame_id: "inner".into(),
            session_id: "oopif".into(),
        };
        let mut refs = RefMap::new();
        for (id, frame) in [("e1", "outer"), ("e2", "inner")] {
            refs.add_with_frame(
                id.into(),
                Some(7),
                "button",
                "shared name",
                None,
                super::super::element::RefContext {
                    frame_id: Some(frame),
                    session_id: Some("oopif"),
                },
            );
        }
        (lifecycle, selected, refs)
    }

    fn event(method: &str, session: &str, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.into(),
            session_id: Some(session.into()),
            params,
        }
    }

    #[test]
    fn selected_frame_rebinds_without_retargeting_refs() {
        let (mut state, mut selected, mut refs) = setup();
        state.apply_event(&mut selected, &mut refs, &event("Target.attachedToTarget", "oopif", json!({"sessionId":"new-owner", "targetInfo":{"type":"iframe", "targetId":"inner"}})));
        assert_eq!(selected.session_id, "new-owner");
        assert_eq!(state.status, SelectionStatus::Pending);
        assert!(
            refs.get("e1").is_some(),
            "parent-document iframe ref survives"
        );
        assert!(
            refs.get("e2").is_none(),
            "old backend IDs must not be rebound"
        );
        state.status = SelectionStatus::Ready;
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Target.detachedFromTarget",
                "page",
                json!({"sessionId":"old-owner"}),
            ),
        );
        assert_eq!(state.status, SelectionStatus::Ready);
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Page.frameDetached",
                "old-owner",
                json!({"frameId":"inner", "reason":"remove"}),
            ),
        );
        assert_eq!(state.status, SelectionStatus::Ready);
    }

    #[test]
    fn selected_frame_removal_is_sticky_but_swap_is_not() {
        let (mut state, mut selected, mut refs) = setup();
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Page.frameDetached",
                "oopif",
                json!({"frameId":"inner", "reason":"swap"}),
            ),
        );
        assert_eq!(state.status, SelectionStatus::Pending);
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Page.frameDetached",
                "page",
                json!({"frameId":"outer", "reason":"remove"}),
            ),
        );
        assert_eq!(state.status, SelectionStatus::Removed);
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Target.attachedToTarget",
                "page",
                json!({"sessionId":"new", "targetInfo":{"type":"iframe", "targetId":"inner"}}),
            ),
        );
        assert_eq!(state.status, SelectionStatus::Removed);
        assert_eq!(selected.session_id, "oopif");
    }

    #[test]
    fn other_tabs_cannot_invalidate_the_selected_frame() {
        let (mut state, mut selected, mut refs) = setup();
        for (id, session) in [("other-frame", "page"), ("inner", "background-session")] {
            state.apply_event(
                &mut selected,
                &mut refs,
                &event(
                    "Page.frameDetached",
                    session,
                    json!({"frameId":id, "reason":"remove"}),
                ),
            );
        }
        assert_eq!(state.status, SelectionStatus::Ready);
        assert!(refs.get("e2").is_some());
    }

    #[test]
    fn navigation_invalidates_only_replaced_document_refs() {
        let (mut state, mut selected, mut refs) = setup();
        state.apply_event(
            &mut selected,
            &mut refs,
            &event(
                "Page.frameNavigated",
                "oopif",
                json!({"frame":{"id":"inner", "loaderId":"new-document"}}),
            ),
        );
        assert!(refs.get("e1").is_some());
        assert!(refs.get("e2").is_none());
        assert_eq!(state.status, SelectionStatus::Pending);
    }

    #[test]
    fn frame_topology_prefers_owning_renderer_for_same_process_descendants() {
        let sessions = HashMap::from([("outer".into(), "oopif".into())]);
        let top_tree = json!({"frame":{"id":"top"}, "childFrames":[{
            "frame":{"id":"outer", "parentId":"top"}, "childFrames":[{
                "frame":{"id":"inner", "parentId":"outer", "loaderId":"old-document"}
            }]
        }]});
        let owner_tree = json!({"frame":{"id":"outer", "parentId":"top"}, "childFrames":[{
            "frame":{"id":"inner", "parentId":"outer", "loaderId":"new-document"}
        }]});
        // Queries can straddle navigation. Prefer the containing renderer's
        // document ID even though this child has no dedicated target session.
        for owner_first in [false, true] {
            let mut targets = HashMap::new();
            let trees = if owner_first {
                [(&owner_tree, "oopif"), (&top_tree, "page")]
            } else {
                [(&top_tree, "page"), (&owner_tree, "oopif")]
            };
            for (tree, session) in trees {
                collect_frame_targets(tree, session, &sessions, &mut targets);
            }
            assert_eq!(targets["inner"].session_id, "oopif");
            assert_eq!(targets["inner"].loader_id.as_deref(), Some("new-document"));
        }
    }

    #[test]
    fn frame_topology_retains_parent_link_when_child_target_is_queried_first() {
        let sessions = HashMap::from([
            ("outer".into(), "outer-session".into()),
            ("inner".into(), "inner-session".into()),
        ]);
        let parent_tree = json!({"frame":{"id":"outer", "parentId":"top"}, "childFrames":[{
            "frame":{"id":"inner", "parentId":"outer", "loaderId":"parent-copy"}
        }]});
        // A dedicated target's root can omit parentId. Its containing
        // renderer supplies the link needed to associate it with this tab.
        let child_tree = json!({"frame":{"id":"inner", "loaderId":"child-document"}});
        for child_first in [false, true] {
            let mut targets = HashMap::new();
            let trees = if child_first {
                [
                    (&child_tree, "inner-session"),
                    (&parent_tree, "outer-session"),
                ]
            } else {
                [
                    (&parent_tree, "outer-session"),
                    (&child_tree, "inner-session"),
                ]
            };
            for (tree, session) in trees {
                collect_frame_targets(tree, session, &sessions, &mut targets);
            }
            assert_eq!(
                targets["inner"].loader_id.as_deref(),
                Some("child-document")
            );
            assert!(frame_reaches_top("inner", "top", &targets));
        }
    }

    #[test]
    fn return_to_same_process_inherits_the_containing_oopif() {
        let tree = json!({"frame":{"id":"outer", "parentId":"top"}, "childFrames":[{"frame":{"id":"inner", "parentId":"outer"}}]});
        for dedicated in [true, false] {
            let sessions = if dedicated {
                HashMap::from([
                    ("outer".into(), "outer-session".into()),
                    ("inner".into(), "inner-session".into()),
                ])
            } else {
                HashMap::from([("outer".into(), "outer-session".into())])
            };
            let mut targets = HashMap::new();
            collect_frame_targets(&tree, "outer-session", &sessions, &mut targets);
            assert_eq!(
                targets["inner"].session_id,
                if dedicated {
                    "inner-session"
                } else {
                    "outer-session"
                }
            );
        }
    }
}
