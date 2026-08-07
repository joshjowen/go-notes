//! The graph view's data.
//!
//! Nodes are notes; edges are resolved links. Unresolved links are included as
//! phantom nodes rather than dropped, because in a linked vault the notes you
//! have *referred to but not yet written* are some of the most useful things the
//! graph can show you — Obsidian draws them too.

use std::collections::{HashMap, HashSet, VecDeque};

use axum::extract::{Query, State};
use axum::Json;
use go_notes_shared::paths;
use go_notes_shared::{EdgeKind, GraphEdge, GraphNode, GraphResponse};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::session::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct GraphParams {
    /// `all` for the whole vault, `local` for a neighbourhood around `path`.
    #[serde(default)]
    pub scope: Option<String>,
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    /// Draw links that point at notes which do not exist yet.
    #[serde(default = "default_true")]
    pub include_unresolved: bool,
    /// Include the model's suggested connections alongside the real links.
    ///
    /// Off unless asked for, and the reason is size rather than taste: this
    /// handler has no `LIMIT` and serialises the whole vault on every open, so
    /// five suggestions a note across two thousand notes is ten thousand extra
    /// edges on a payload that was fine as it was.
    #[serde(default)]
    pub semantic: bool,
}

fn default_true() -> bool {
    true
}

/// A note as loaded from the database, before being numbered for the wire.
struct RawNode {
    id: Uuid,
    path: String,
    title: String,
    tags: Vec<String>,
}

pub async fn graph(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(params): Query<GraphParams>,
) -> AppResult<Json<GraphResponse>> {
    let rows = sqlx::query(
        "SELECT n.id, n.rel_path, n.title,
                COALESCE(
                    (SELECT array_agg(t.name ORDER BY t.name)
                     FROM note_tags nt JOIN tags t ON t.id = nt.tag_id
                     WHERE nt.note_id = n.id),
                    ARRAY[]::text[]
                ) AS tags
         FROM notes n
         WHERE n.user_id = $1",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;

    let mut notes = Vec::with_capacity(rows.len());
    for row in rows {
        notes.push(RawNode {
            id: row.try_get("id")?,
            path: row.try_get("rel_path")?,
            title: row.try_get("title")?,
            tags: row.try_get("tags")?,
        });
    }

    // Resolved edges, plus the raw targets of broken links so they can become
    // phantom nodes. `DISTINCT` because two notes commonly link to the same
    // place more than once, and a doubled edge just makes the layout heavier.
    //
    // `relation` joins the DISTINCT rather than being dropped, so two links
    // between the same pair that say different things stay two edges — the word
    // is the whole point of a typed link, and collapsing them would keep an
    // arbitrary one. `NULLS FIRST` puts the untyped row first for a pair that
    // has both, so the plain link wins the dedup below and the graph does not
    // claim a relationship the author only wrote once.
    let link_rows = sqlx::query(
        "SELECT DISTINCT l.source_note_id, l.target_note_id, l.target_raw, l.target_key,
                l.relation
         FROM links l
         WHERE l.user_id = $1
         ORDER BY l.relation NULLS FIRST",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;

    // Number the nodes: the wire format uses small integers rather than UUIDs,
    // which for a vault of a few thousand notes is a large saving on a payload
    // the frontend fetches every time the graph opens.
    let mut index_of: HashMap<Uuid, u32> = HashMap::with_capacity(notes.len());
    let mut nodes: Vec<GraphNode> = Vec::with_capacity(notes.len());

    for note in &notes {
        let index = nodes.len() as u32;
        index_of.insert(note.id, index);
        nodes.push(GraphNode {
            id: index,
            folder: paths::parent_of(&note.path).to_string(),
            path: note.path.clone(),
            title: note.title.clone(),
            tags: note.tags.clone(),
            degree: 0,
            unresolved: false,
        });
    }

    // Phantom nodes for link targets with no note behind them, one per distinct
    // target rather than one per link.
    let mut phantom_of: HashMap<String, u32> = HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_edges: HashSet<(u32, u32, Option<String>)> = HashSet::new();
    // Unordered pairs that already have a real link, so a suggestion never
    // duplicates a connection the author made themselves, and unordered pairs
    // that already have a suggestion, since similarity is symmetric.
    let mut linked_pairs: HashSet<(u32, u32)> = HashSet::new();
    let mut seen_semantic: HashSet<(u32, u32)> = HashSet::new();

    for row in link_rows {
        let source_id: Uuid = row.try_get("source_note_id")?;
        let Some(&source) = index_of.get(&source_id) else {
            continue;
        };

        let target_note: Option<Uuid> = row.try_get("target_note_id")?;
        let target = match target_note {
            Some(id) => match index_of.get(&id) {
                Some(&index) => index,
                None => continue,
            },
            None => {
                if !params.include_unresolved {
                    continue;
                }
                let key: String = row.try_get("target_key")?;
                let raw: String = row.try_get("target_raw")?;
                match phantom_of.get(&key) {
                    Some(&index) => index,
                    None => {
                        let index = nodes.len() as u32;
                        phantom_of.insert(key, index);
                        nodes.push(GraphNode {
                            id: index,
                            path: raw.clone(),
                            title: paths::basename(&raw).to_string(),
                            folder: String::new(),
                            tags: Vec::new(),
                            degree: 0,
                            unresolved: true,
                        });
                        index
                    }
                }
            }
        };

        // A note linking to itself is common in templates and adds nothing but
        // a self-loop the layout cannot draw sensibly.
        let relation: Option<String> = row.try_get("relation")?;
        if source == target || !seen_edges.insert((source, target, relation.clone())) {
            continue;
        }
        linked_pairs.insert((source.min(target), source.max(target)));
        edges.push(GraphEdge {
            source,
            target,
            kind: match relation {
                Some(_) => EdgeKind::Typed,
                None => EdgeKind::Link,
            },
            relation,
            weight: 1.0,
        });
    }

    if params.semantic {
        let suggested = sqlx::query(
            "SELECT s.source_note_id, s.target_note_id, s.score
             FROM semantic_links s
             WHERE s.user_id = $1
             ORDER BY s.score DESC",
        )
        .bind(user.id)
        .fetch_all(&state.pool)
        .await?;

        for row in suggested {
            let source_id: Uuid = row.try_get("source_note_id")?;
            let target_id: Uuid = row.try_get("target_note_id")?;
            let (Some(&source), Some(&target)) =
                (index_of.get(&source_id), index_of.get(&target_id))
            else {
                continue;
            };
            // Similarity is symmetric, so A→B and B→A are the same suggestion
            // seen twice; and a pair that is already linked does not need the
            // model to point out that they are related.
            let pair = (source.min(target), source.max(target));
            if source == target
                || linked_pairs.contains(&pair)
                || !seen_semantic.insert(pair)
            {
                continue;
            }
            edges.push(GraphEdge {
                source,
                target,
                kind: EdgeKind::Semantic,
                relation: None,
                weight: row.try_get::<f32, _>("score")?,
            });
        }
    }

    for edge in &edges {
        nodes[edge.source as usize].degree += 1;
        nodes[edge.target as usize].degree += 1;
    }

    // Local scope: keep only the neighbourhood of one note.
    if params.scope.as_deref() == Some("local") {
        if let Some(focus) = params.path.as_deref() {
            let depth = params.depth.unwrap_or(1).min(5);
            return Ok(Json(restrict_to_neighbourhood(nodes, edges, focus, depth)));
        }
    }

    Ok(Json(GraphResponse { nodes, edges }))
}

/// Trims the graph to the notes within `depth` hops of `focus`.
///
/// Traversal is undirected: a note that links *to* the focused note is just as
/// much a neighbour as one it links to, and a local graph that showed only
/// outbound links would miss most of what the user is looking for.
fn restrict_to_neighbourhood(
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    focus: &str,
    depth: u32,
) -> GraphResponse {
    let Some(start) = nodes.iter().position(|node| node.path == focus) else {
        return GraphResponse {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    };

    let mut neighbours: HashMap<u32, Vec<u32>> = HashMap::new();
    for edge in &edges {
        neighbours.entry(edge.source).or_default().push(edge.target);
        neighbours.entry(edge.target).or_default().push(edge.source);
    }

    let mut kept: HashSet<u32> = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back((start as u32, 0u32));
    kept.insert(start as u32);

    while let Some((node, distance)) = queue.pop_front() {
        if distance >= depth {
            continue;
        }
        for &next in neighbours.get(&node).map(Vec::as_slice).unwrap_or(&[]) {
            if kept.insert(next) {
                queue.push_back((next, distance + 1));
            }
        }
    }

    // Renumber, because the frontend indexes into the node array directly.
    let mut renumbered: HashMap<u32, u32> = HashMap::with_capacity(kept.len());
    let mut out_nodes = Vec::with_capacity(kept.len());
    for (index, mut node) in nodes.into_iter().enumerate() {
        if !kept.contains(&(index as u32)) {
            continue;
        }
        let new_index = out_nodes.len() as u32;
        renumbered.insert(index as u32, new_index);
        node.id = new_index;
        out_nodes.push(node);
    }

    let out_edges = edges
        .into_iter()
        .filter_map(|edge| {
            Some(GraphEdge {
                source: *renumbered.get(&edge.source)?,
                target: *renumbered.get(&edge.target)?,
                ..edge
            })
        })
        .collect();

    GraphResponse {
        nodes: out_nodes,
        edges: out_edges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, path: &str) -> GraphNode {
        GraphNode {
            id,
            path: path.into(),
            title: path.into(),
            folder: String::new(),
            tags: Vec::new(),
            degree: 0,
            unresolved: false,
        }
    }

    fn edge(source: u32, target: u32) -> GraphEdge {
        GraphEdge {
            source,
            target,
            kind: EdgeKind::Link,
            relation: None,
            weight: 1.0,
        }
    }

    /// A ── B ── C     D (isolated)
    fn chain() -> (Vec<GraphNode>, Vec<GraphEdge>) {
        (
            vec![node(0, "A.md"), node(1, "B.md"), node(2, "C.md"), node(3, "D.md")],
            vec![edge(0, 1), edge(1, 2)],
        )
    }

    #[test]
    fn depth_one_keeps_immediate_neighbours() {
        let (nodes, edges) = chain();
        let result = restrict_to_neighbourhood(nodes, edges, "A.md", 1);

        let paths: Vec<&str> = result.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, vec!["A.md", "B.md"]);
        assert_eq!(result.edges.len(), 1);
    }

    #[test]
    fn depth_two_reaches_further() {
        let (nodes, edges) = chain();
        let result = restrict_to_neighbourhood(nodes, edges, "A.md", 2);

        let paths: Vec<&str> = result.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, vec!["A.md", "B.md", "C.md"]);
        assert_eq!(result.edges.len(), 2);
    }

    /// The property that makes a local graph useful: what links *to* you counts
    /// as a neighbour, not just what you link to.
    #[test]
    fn traversal_follows_links_in_both_directions() {
        let (nodes, edges) = chain();
        // C is only ever a link *target*, so a directed walk would find nothing.
        let result = restrict_to_neighbourhood(nodes, edges, "C.md", 1);

        let paths: Vec<&str> = result.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, vec!["B.md", "C.md"]);
    }

    #[test]
    fn renumbering_keeps_edges_pointing_at_the_right_nodes() {
        let (nodes, edges) = chain();
        let result = restrict_to_neighbourhood(nodes, edges, "B.md", 1);

        // Every edge must land inside the returned array.
        for edge in &result.edges {
            assert!((edge.source as usize) < result.nodes.len());
            assert!((edge.target as usize) < result.nodes.len());
        }
        // And ids must match array positions, which the frontend relies on.
        for (index, node) in result.nodes.iter().enumerate() {
            assert_eq!(node.id, index as u32);
        }
    }

    #[test]
    fn an_isolated_note_yields_just_itself() {
        let (nodes, edges) = chain();
        let result = restrict_to_neighbourhood(nodes, edges, "D.md", 3);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].path, "D.md");
        assert!(result.edges.is_empty());
    }

    #[test]
    fn an_unknown_focus_yields_nothing_rather_than_panicking() {
        let (nodes, edges) = chain();
        let result = restrict_to_neighbourhood(nodes, edges, "Missing.md", 2);
        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
    }

    /// A cycle must terminate the traversal rather than loop forever.
    #[test]
    fn cycles_terminate() {
        let nodes = vec![node(0, "A.md"), node(1, "B.md"), node(2, "C.md")];
        let edges = vec![edge(0, 1), edge(1, 2), edge(2, 0)];
        let result = restrict_to_neighbourhood(nodes, edges, "A.md", 5);
        assert_eq!(result.nodes.len(), 3);
        assert_eq!(result.edges.len(), 3);
    }
}
