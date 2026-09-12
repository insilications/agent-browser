//! Frame topology shared by accessibility audits and active-tab session tracking.
//! Renderer trees supply document ownership; parent-side trees connect OOPIFs
//! to their containing frames regardless of target query order.
use super::cdp::client::CdpClient;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub(super) struct FrameTopology {
    pub top_frame_id: String,
    pub frames: HashMap<String, FrameTarget>,
    /// A failed target query is uncertainty, not evidence of frame removal.
    pub incomplete: bool,
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
