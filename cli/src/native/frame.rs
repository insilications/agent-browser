use std::collections::{HashMap, HashSet};

use futures_util::future::join_all;
use serde_json::Value;

use super::cdp::client::CdpClient;

pub const SELECTED_FRAME_UNAVAILABLE: &str =
    "Selected frame is no longer available. Run `agent-browser frame main`, then select the frame again.";

/// A browsing context and the CDP target session that can execute it.
/// Same-process frames inherit the nearest ancestor target's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameContext {
    pub frame_id: String,
    pub session_id: String,
}

/// Persistent frame selection state. `Unavailable` deliberately retains the
/// frame identity instead of falling back to the main document. A renderer
/// swap can replace an `Available` context during the reconciliation grace
/// period; once marked `Unavailable`, selection stays sticky until reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedFrame {
    Available(FrameContext),
    Unavailable {
        frame_id: String,
        last_session_id: String,
    },
}

impl SelectedFrame {
    pub fn frame_id(&self) -> &str {
        match self {
            Self::Available(context) => &context.frame_id,
            Self::Unavailable { frame_id, .. } => frame_id,
        }
    }

    pub fn context(&self) -> Result<&FrameContext, String> {
        match self {
            Self::Available(context) => Ok(context),
            Self::Unavailable { .. } => Err(SELECTED_FRAME_UNAVAILABLE.to_string()),
        }
    }

    pub fn last_session_id(&self) -> &str {
        match self {
            Self::Available(context) => &context.session_id,
            Self::Unavailable {
                last_session_id, ..
            } => last_session_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameTarget {
    pub frame_id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FrameTopology {
    pub top_frame_id: String,
    targets: HashMap<String, FrameTarget>,
}

impl FrameTopology {
    pub fn frame_context(&self, frame_id: &str) -> Option<FrameContext> {
        self.targets.get(frame_id).map(|target| FrameContext {
            frame_id: target.frame_id.clone(),
            session_id: target.session_id.clone(),
        })
    }

    pub fn active_iframe_session_ids(&self, top_session_id: &str) -> HashSet<String> {
        self.targets
            .values()
            .filter(|target| target.session_id != top_session_id)
            .map(|target| target.session_id.clone())
            .collect()
    }

    pub fn targets(&self) -> Vec<FrameTarget> {
        let mut targets = self.targets.values().cloned().collect::<Vec<_>>();
        targets.sort_unstable_by(|left, right| left.frame_id.cmp(&right.frame_id));
        targets
    }
}

fn collect_frame_targets(
    tree: &Value,
    parent_session_id: &str,
    iframe_sessions: &HashMap<String, String>,
    targets: &mut HashMap<String, FrameTarget>,
) {
    let session_id = if let Some(frame) = tree.get("frame") {
        let Some(frame_id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let session_id = iframe_sessions
            .get(frame_id)
            .cloned()
            .unwrap_or_else(|| parent_session_id.to_string());
        let target = FrameTarget {
            frame_id: frame_id.to_string(),
            session_id: session_id.clone(),
            parent_id: frame
                .get("parentId")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        };
        targets
            .entry(frame_id.to_string())
            .and_modify(|existing| {
                // The dedicated target is authoritative for execution session
                // and descendants, but its root often omits its external parent.
                if iframe_sessions.get(frame_id) == Some(&session_id) {
                    let mut authoritative = target.clone();
                    if authoritative.parent_id.is_none() {
                        authoritative.parent_id.clone_from(&existing.parent_id);
                    }
                    *existing = authoritative;
                }
            })
            .or_insert(target);
        session_id
    } else {
        parent_session_id.to_string()
    };

    if let Some(children) = tree.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frame_targets(child, &session_id, iframe_sessions, targets);
        }
    }
}

fn frame_reaches_top(
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

/// Reconstruct the active page's frame topology from every renderer target.
/// The top target can omit descendants below an OOPIF, so dedicated iframe
/// sessions are queried as peers and joined through their parent frame IDs.
pub async fn collect_active_frame_topology(
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
        .and_then(Value::as_str)
        .ok_or("Could not determine top-level frame ID")?
        .to_string();

    let mut targets = HashMap::new();
    if let Some(tree) = top_tree.get("frameTree") {
        collect_frame_targets(tree, top_session_id, iframe_sessions, &mut targets);
    }

    let mut session_ids = iframe_sessions.values().cloned().collect::<Vec<_>>();
    session_ids.sort_unstable();
    session_ids.dedup();
    let trees = join_all(session_ids.iter().map(|session_id| async move {
        let result = client
            .send_command_no_params("Page.getFrameTree", Some(session_id))
            .await
            .ok()?;
        Some((session_id.as_str(), result.get("frameTree")?.clone()))
    }))
    .await;
    for (session_id, tree) in trees.into_iter().flatten() {
        collect_frame_targets(&tree, session_id, iframe_sessions, &mut targets);
    }

    let active_frame_ids = targets
        .keys()
        .filter(|frame_id| frame_reaches_top(frame_id, &top_frame_id, &targets))
        .cloned()
        .collect::<HashSet<_>>();
    targets.retain(|frame_id, _| active_frame_ids.contains(frame_id));
    Ok(FrameTopology {
        top_frame_id,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame_target(frame_id: &str, session_id: &str, parent_id: Option<&str>) -> FrameTarget {
        FrameTarget {
            frame_id: frame_id.to_string(),
            session_id: session_id.to_string(),
            parent_id: parent_id.map(ToString::to_string),
        }
    }

    #[test]
    fn selected_frame_reports_unavailable_without_losing_identity() {
        let selected = SelectedFrame::Unavailable {
            frame_id: "frame-a".to_string(),
            last_session_id: "session-a".to_string(),
        };
        assert_eq!(selected.frame_id(), "frame-a");
        assert_eq!(selected.last_session_id(), "session-a");
        assert_eq!(selected.context().unwrap_err(), SELECTED_FRAME_UNAVAILABLE);
    }

    #[test]
    fn collect_frame_targets_inherits_nearest_ancestor_session() {
        let tree = json!({
            "frame": { "id": "top" },
            "childFrames": [{
                "frame": { "id": "oopif", "parentId": "top" },
                "childFrames": [{
                    "frame": { "id": "same-process-child", "parentId": "oopif" }
                }]
            }]
        });
        let iframe_sessions = HashMap::from([("oopif".to_string(), "oopif-session".to_string())]);
        let mut targets = HashMap::new();

        collect_frame_targets(&tree, "top-session", &iframe_sessions, &mut targets);

        assert_eq!(targets["top"].session_id, "top-session");
        assert_eq!(targets["oopif"].session_id, "oopif-session");
        assert_eq!(targets["same-process-child"].session_id, "oopif-session");
        assert_eq!(
            targets["same-process-child"].parent_id.as_deref(),
            Some("oopif")
        );
    }

    #[test]
    fn frame_reaches_top_filters_background_frames() {
        let targets = HashMap::from([
            ("top".to_string(), frame_target("top", "top-session", None)),
            (
                "active-child".to_string(),
                frame_target("active-child", "active-session", Some("top")),
            ),
            (
                "background".to_string(),
                frame_target("background", "background-session", None),
            ),
            (
                "background-child".to_string(),
                frame_target("background-child", "background-session", Some("background")),
            ),
        ]);

        assert!(frame_reaches_top("active-child", "top", &targets));
        assert!(!frame_reaches_top("background-child", "top", &targets));
    }

    #[test]
    fn topology_resolves_session_transitions_by_stable_frame_id() {
        let mut targets = HashMap::from([(
            "child".to_string(),
            frame_target("child", "top-session", Some("top")),
        )]);
        let same_process = FrameTopology {
            top_frame_id: "top".to_string(),
            targets: targets.clone(),
        };
        assert_eq!(
            same_process.frame_context("child").unwrap().session_id,
            "top-session"
        );

        targets.get_mut("child").unwrap().session_id = "oopif-session".to_string();
        let oopif = FrameTopology {
            top_frame_id: "top".to_string(),
            targets,
        };
        assert_eq!(
            oopif.frame_context("child").unwrap().session_id,
            "oopif-session"
        );
    }
}
