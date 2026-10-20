//! Graph commands.
//!
//! Commands are thin: they validate, read the projection and return a small
//! answer. The graph itself streams to the front end as `gilde://graph/patch`;
//! `graph_snapshot` exists for first paint and for revision-gap resync.

use serde::{Deserialize, Serialize};

use crate::error::{GildeError, GildeErrorPayload, CmdResult};
use crate::model::graph::{parse_node_id, GraphHandle, GraphSnapshot, GraphState, NodeKind};

/// What `graph_focus` resolved a node to. The front end uses this to reveal or
/// open the matching panel — the graph is a navigation surface, not a poster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusResult {
    pub kind: NodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn unknown_node(node_id: &str) -> GildeErrorPayload {
    GildeError::not_found(format!("graph node {node_id}")).into()
}

/// Walk from `node_id` up the `parentId` chain, collecting the first ancestor
/// of each interesting kind. Bounded so a corrupt cycle cannot hang a command.
fn resolve_focus(state: &GraphState, node_id: &str) -> Option<FocusResult> {
    let (kind, entity_id) = parse_node_id(node_id)?;
    let node = state.node(node_id)?;

    let mut result = FocusResult {
        kind,
        session_id: None,
        worktree_id: None,
        task_id: None,
    };
    match kind {
        NodeKind::Session => result.session_id = Some(entity_id.to_owned()),
        NodeKind::Worktree => result.worktree_id = Some(entity_id.to_owned()),
        NodeKind::Dispatch => result.task_id = Some(entity_id.to_owned()),
        NodeKind::Run | NodeKind::Project | NodeKind::Account | NodeKind::Orphan => {}
    }

    // A session or run node records its worktree directly.
    if result.worktree_id.is_none() {
        if let Some(worktree_id) = node.meta.worktree_id.as_ref() {
            result.worktree_id = Some(worktree_id.clone());
        }
    }

    let mut cursor = node.parent_id.clone();
    let mut hops = 0;
    while let Some(parent_id) = cursor {
        hops += 1;
        if hops > 16 {
            break;
        }
        let Some((parent_kind, parent_entity)) = parse_node_id(&parent_id) else {
            break;
        };
        match parent_kind {
            NodeKind::Worktree if result.worktree_id.is_none() => {
                result.worktree_id = Some(parent_entity.to_owned())
            }
            NodeKind::Dispatch if result.task_id.is_none() => {
                result.task_id = Some(parent_entity.to_owned())
            }
            NodeKind::Session if result.session_id.is_none() => {
                result.session_id = Some(parent_entity.to_owned())
            }
            _ => {}
        }
        cursor = state.node(&parent_id).and_then(|n| n.parent_id.clone());
    }

    Some(result)
}

/// Full graph, for first paint and for `rev` gap recovery.
#[tauri::command]
pub async fn graph_snapshot(graph: tauri::State<'_, GraphHandle>) -> CmdResult<GraphSnapshot> {
    Ok(graph.snapshot())
}

/// Resolve a node id to the entities the UI should reveal.
#[tauri::command]
pub async fn graph_focus(
    node_id: String,
    graph: tauri::State<'_, GraphHandle>,
) -> CmdResult<FocusResult> {
    let state = graph.state().read();
    resolve_focus(&state, &node_id).ok_or_else(|| unknown_node(&node_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::graph::{GraphNode, GraphOp, NodeStatus};

    fn seeded() -> GraphState {
        let mut state = GraphState::new();
        let mut project = GraphNode::new(
            "project:p1",
            NodeKind::Project,
            "gilde-code",
            NodeStatus::Ok,
            1,
        );
        project.parent_id = None;
        state.apply(GraphOp::UpsertNode { node: project });

        let mut worktree = GraphNode::new(
            "worktree:w1",
            NodeKind::Worktree,
            "fix-pty-resize",
            NodeStatus::Dirty,
            1,
        );
        worktree.parent_id = Some("project:p1".into());
        state.apply(GraphOp::UpsertNode { node: worktree });

        let mut session = GraphNode::new(
            "session:s1",
            NodeKind::Session,
            "claude",
            NodeStatus::Working,
            1,
        );
        session.parent_id = Some("worktree:w1".into());
        session.meta.worktree_id = Some("w1".into());
        state.apply(GraphOp::UpsertNode { node: session });

        let mut dispatch = GraphNode::new(
            "dispatch:t1",
            NodeKind::Dispatch,
            "add tests",
            NodeStatus::Working,
            1,
        );
        dispatch.parent_id = Some("project:p1".into());
        state.apply(GraphOp::UpsertNode { node: dispatch });

        let mut run = GraphNode::new("run:r1", NodeKind::Run, "codex", NodeStatus::Working, 1);
        run.parent_id = Some("dispatch:t1".into());
        run.meta.worktree_id = Some("w1".into());
        state.apply(GraphOp::UpsertNode { node: run });

        state
    }

    #[test]
    fn focus_resolves_a_session_to_its_worktree() {
        let state = seeded();
        let focus = resolve_focus(&state, "session:s1").unwrap();
        assert_eq!(focus.kind, NodeKind::Session);
        assert_eq!(focus.session_id.as_deref(), Some("s1"));
        assert_eq!(focus.worktree_id.as_deref(), Some("w1"));
        assert_eq!(focus.task_id, None);
    }

    #[test]
    fn focus_resolves_a_run_to_its_task_and_worktree() {
        let state = seeded();
        let focus = resolve_focus(&state, "run:r1").unwrap();
        assert_eq!(focus.kind, NodeKind::Run);
        assert_eq!(focus.task_id.as_deref(), Some("t1"));
        assert_eq!(focus.worktree_id.as_deref(), Some("w1"));
    }

    #[test]
    fn focus_resolves_a_worktree_and_a_project() {
        let state = seeded();
        assert_eq!(
            resolve_focus(&state, "worktree:w1").unwrap().worktree_id,
            Some("w1".to_owned())
        );
        let project = resolve_focus(&state, "project:p1").unwrap();
        assert_eq!(project.kind, NodeKind::Project);
        assert_eq!(project.worktree_id, None);
    }

    #[test]
    fn focus_rejects_unknown_and_malformed_ids() {
        let state = seeded();
        assert!(resolve_focus(&state, "session:nope").is_none());
        assert!(resolve_focus(&state, "banana:1").is_none());
        let err = unknown_node("session:nope");
        assert_eq!(err.code, "NOT_FOUND");
        assert_eq!(err.key, "error.notFound");
        assert_eq!(err.params["what"], "graph node session:nope");
        assert!(!err.retryable);
    }

    #[test]
    fn focus_survives_a_parent_cycle() {
        let mut state = GraphState::new();
        let mut a = GraphNode::new("session:a", NodeKind::Session, "a", NodeStatus::Idle, 1);
        a.parent_id = Some("session:b".into());
        let mut b = GraphNode::new("session:b", NodeKind::Session, "b", NodeStatus::Idle, 1);
        b.parent_id = Some("session:a".into());
        state.apply(GraphOp::UpsertNode { node: a });
        state.apply(GraphOp::UpsertNode { node: b });
        assert!(resolve_focus(&state, "session:a").is_some());
    }
}
