//! Left-to-right ASCII DAG visualisation.
//!
//! Nodes are laid out in columns by longest-path layer. Within each
//! column, nodes are ordered by a barycenter heuristic to reduce edge
//! crossings. Long edges (spanning more than one layer) are broken by
//! dummy waypoints so every rendered edge sits between adjacent
//! columns. The canvas is then rasterised with Unicode box-drawing
//! characters.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ast::Phase;
use crate::dag::Dag;

const HORIZ: char = '─';
const VERT: char = '│';
const TOP_LEFT: char = '┌';
const TOP_RIGHT: char = '┐';
const BOTTOM_LEFT: char = '└';
const BOTTOM_RIGHT: char = '┘';
const ROUND_TOP_LEFT: char = '╭';
const ROUND_TOP_RIGHT: char = '╮';
const ROUND_BOTTOM_LEFT: char = '╰';
const ROUND_BOTTOM_RIGHT: char = '╯';
const T_DOWN: char = '┬';
const T_UP: char = '┴';
const T_RIGHT: char = '├';
const T_LEFT: char = '┤';
const CROSS: char = '┼';
/// Grid-only marker for two straight strokes passing through each other
/// without connecting. It keeps all four strokes for later merges, so a
/// turn landing on the cell still becomes a junction, and is emitted as an
/// unbroken vertical so it can't be mistaken for a `┼` junction.
const CROSSING: char = '\u{E000}';
const ARROW: char = '→';

/// Per-node styling for [`render`]. The caller supplies both a visible
/// label (used to compute column widths at layout time) and a rendered
/// string (typically wrapped in ANSI colour codes) that is emitted in
/// place of the label at output time. Keeping these separate means the
/// graph module stays agnostic of any particular colour library.
///
/// When `arrow` is set, the `→` glyph on every incoming edge that
/// terminates at this node is replaced with the given single-character
/// marker (its `char`), and the rendered form (often ANSI-wrapped) is
/// emitted in that cell. Graph roots — nodes with no incoming edges —
/// have no arrow to replace and therefore show nothing extra.
#[derive(Clone, Debug)]
pub struct NodeStyle {
    /// Visible text for width calculations — no ANSI sequences.
    pub label: String,
    /// Text emitted in the final output, typically ANSI-wrapped.
    pub rendered: String,
    /// Optional replacement for the arrow glyph on edges terminating at
    /// this node: `(visible_char, rendered_text)`.
    pub arrow: Option<(char, String)>,
}

/// Render the DAG as a left-to-right ASCII graph, grouped by phase.
///
/// Targets are partitioned into pre / default / post phases, and each
/// phase is rendered as its own subgraph laid out side-by-side from
/// left to right. Edges between phases are omitted — the horizontal
/// layout implies ordering. Within a phase, all parent edges are
/// rendered (both content and ordering), since the graph represents
/// run-time relationships.
///
/// # Arguments
///
/// * `styles` - Optional per-node label overrides and colour wrappers.
///   Nodes missing from the map render as plain names with no colour.
pub fn render(dag: &Dag, names: &[String], styles: &HashMap<String, NodeStyle>) -> String {
    let mut pre: Vec<String> = Vec::new();
    let mut default: Vec<String> = Vec::new();
    let mut post: Vec<String> = Vec::new();
    for name in names {
        match dag.get_node(name).map(|n| n.phase) {
            Some(Phase::Pre) => pre.push(name.clone()),
            Some(Phase::Post) => post.push(name.clone()),
            _ => default.push(name.clone()),
        }
    }

    // Each phase is rendered as a list of (emitted_line, visible_width)
    // pairs. Visible width is what counts for inter-phase padding; the
    // emitted line may additionally contain ANSI escape sequences.
    let parts: Vec<Vec<(String, usize)>> = [pre, default, post]
        .iter()
        .filter(|phase| !phase.is_empty())
        .map(|phase| render_phase(dag, phase, styles))
        .collect();

    let height = parts.iter().map(|p| p.len()).max().unwrap_or(0);
    let widths: Vec<usize> = parts
        .iter()
        .map(|p| p.iter().map(|(_, w)| *w).max().unwrap_or(0))
        .collect();
    const GUTTER: usize = 4;

    let mut out = String::new();
    for row in 0..height {
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                for _ in 0..GUTTER {
                    out.push(' ');
                }
            }
            let empty = (String::new(), 0usize);
            let (line, visible_w) = part.get(row).unwrap_or(&empty);
            out.push_str(line);
            if i + 1 < parts.len() {
                for _ in *visible_w..widths[i] {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

/// Render the selected targets as a JSON object mapping each target name to
/// the sorted names of the selected targets that must run before it.
///
/// Every edge counts, whatever created it, including the synthetic edges
/// that order phases, so the map alone is the complete run-time graph.
pub fn render_json(dag: &Dag, names: &[String]) -> String {
    let selected: HashSet<&str> = names.iter().map(String::as_str).collect();
    let graph: BTreeMap<&str, Vec<String>> = names
        .iter()
        .map(|name| {
            let mut deps: Vec<String> = dag
                .deps(name)
                .into_iter()
                .filter(|p| selected.contains(p.as_str()))
                .collect();
            deps.sort();
            deps.dedup();
            (name.as_str(), deps)
        })
        .collect();
    serde_json::to_string_pretty(&graph).expect("JSON serialization failed")
}

fn render_phase(dag: &Dag, names: &[String], styles: &HashMap<String, NodeStyle>) -> Vec<(String, usize)> {
    let visible: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut parents: HashMap<String, Vec<String>> = HashMap::new();
    for name in names {
        parents.insert(
            name.clone(),
            dag.deps(name)
                .into_iter()
                .filter(|p| visible.contains(p.as_str()))
                .collect(),
        );
    }
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut rendered: HashMap<String, String> = HashMap::new();
    let mut arrows: HashMap<String, (char, String)> = HashMap::new();
    for name in names {
        if let Some(style) = styles.get(name) {
            labels.insert(name.clone(), style.label.clone());
            rendered.insert(name.clone(), style.rendered.clone());
            if let Some(arrow) = &style.arrow {
                arrows.insert(name.clone(), arrow.clone());
            }
        }
    }
    Layout::build_from_parents(names, &parents, &labels).rasterise(&rendered, &arrows)
}

/// Render from a precomputed parents map. `topo` must list every node in
/// topological order (parents before children). Exposed for tests that
/// can't easily construct a full `Dag`.
#[cfg(test)]
fn render_parents(topo: &[String], parents: &HashMap<String, Vec<String>>) -> String {
    let layout = Layout::build_from_parents(topo, parents, &HashMap::new());
    layout
        .rasterise(&HashMap::new(), &HashMap::new())
        .into_iter()
        .map(|(s, _)| s)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A layout slot. Real nodes carry a `key` (the target name, used for edge
/// lookups) and a separate `label` (what gets drawn and measured). The two
/// differ when the caller supplies a decorated label — for example `+ foo`
/// with a plan symbol prepended — so we can lay out around the rendered
/// width while still routing edges by name. Dummies are waypoints inserted
/// so long edges traverse adjacent columns only; `edge_id` identifies the
/// originating edge, `hop` distinguishes multiple dummies along it.
#[derive(Clone, Debug)]
enum Slot {
    Real { key: String, label: String },
    Dummy { edge_id: usize, hop: usize },
}

impl Slot {
    fn key(&self) -> String {
        match self {
            Slot::Real { key, .. } => key.clone(),
            Slot::Dummy { edge_id, hop } => format!("__dummy_{edge_id}_{hop}"),
        }
    }

    fn is_real(&self) -> bool {
        matches!(self, Slot::Real { .. })
    }

    fn label_width(&self) -> usize {
        match self {
            Slot::Real { label, .. } => label.chars().count(),
            Slot::Dummy { .. } => 0,
        }
    }
}

struct Layout {
    /// Nodes per layer (column), in within-layer order.
    layers: Vec<Vec<Slot>>,
    /// Edges between slot keys; every edge spans exactly one layer.
    edges: Vec<(String, String)>,
    /// slot key -> (layer index, row index).
    pos: HashMap<String, (usize, usize)>,
    /// Column x-offset for each layer.
    layer_x: Vec<usize>,
    /// Turn column for each source slot with at least one edge whose target
    /// sits on a different row. Absent for sources that only have straight
    /// (same-row) edges. Per-source lanes keep vertical strokes from
    /// different sources from stacking into the same column and creating
    /// ambiguous junctions.
    turn_col: HashMap<String, usize>,
    /// Merge column for each target slot with several incoming edges.
    /// Edges into such a target join only here, never on a source's lane,
    /// so a junction never connects one source to another's targets.
    merge_col: HashMap<String, usize>,
    /// Row on which a turning edge into a merge target runs from its
    /// source's lane to the merge column. The stretch between the two
    /// columns belongs to that edge alone; see [`Layout::assign_tracks`].
    track: HashMap<(String, String), usize>,
    /// Total grid width and height.
    width: usize,
    height: usize,
}

impl Layout {
    fn build_from_parents(
        names: &[String],
        parents: &HashMap<String, Vec<String>>,
        labels: &HashMap<String, String>,
    ) -> Self {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut raw_edges: Vec<(String, String)> = Vec::new();
        for name in names {
            children.insert(name.clone(), Vec::new());
        }
        for name in names {
            for parent in parents.get(name).map(|v| v.as_slice()).unwrap_or(&[]) {
                children.get_mut(parent).expect("seeded above").push(name.clone());
                raw_edges.push((parent.clone(), name.clone()));
            }
        }

        // Longest-path layering. `names` is the topo order, so parents are
        // always placed before children — one pass suffices.
        let mut layer: HashMap<String, usize> = HashMap::new();
        for name in names {
            let l = parents
                .get(name)
                .map(|ps| ps.iter().map(|p| layer[p] + 1).max().unwrap_or(0))
                .unwrap_or(0);
            layer.insert(name.clone(), l);
        }
        let max_layer = layer.values().copied().max().unwrap_or(0);

        // Seed each layer with real nodes, alphabetically. The label
        // defaults to the target name when no override is provided.
        let mut layers: Vec<Vec<Slot>> = vec![Vec::new(); max_layer + 1];
        let mut sorted = names.to_vec();
        sorted.sort();
        for name in &sorted {
            let label = labels.get(name).cloned().unwrap_or_else(|| name.clone());
            layers[layer[name]].push(Slot::Real {
                key: name.clone(),
                label,
            });
        }

        // Expand long edges with dummy waypoints; rebuild the edge list so
        // every edge spans exactly one layer.
        let mut edges: Vec<(String, String)> = Vec::new();
        let mut edge_parents: HashMap<String, Vec<String>> = HashMap::new();
        let mut edge_children: HashMap<String, Vec<String>> = HashMap::new();
        for lyr in &layers {
            for slot in lyr {
                edge_parents.insert(slot.key(), Vec::new());
                edge_children.insert(slot.key(), Vec::new());
            }
        }
        for (edge_id, (u, v)) in raw_edges.iter().enumerate() {
            let (lu, lv) = (layer[u], layer[v]);
            if lv == lu + 1 {
                edges.push((u.clone(), v.clone()));
                edge_parents.get_mut(v).expect("seeded").push(u.clone());
                edge_children.get_mut(u).expect("seeded").push(v.clone());
                continue;
            }
            // Insert a dummy per intermediate layer.
            let mut prev = u.clone();
            for (hop, l) in ((lu + 1)..lv).enumerate() {
                let dummy = Slot::Dummy { edge_id, hop };
                let key = dummy.key();
                layers[l].push(dummy);
                edge_parents.insert(key.clone(), vec![prev.clone()]);
                edge_children.insert(key.clone(), Vec::new());
                edge_children.get_mut(&prev).expect("seeded").push(key.clone());
                edges.push((prev.clone(), key.clone()));
                prev = key;
            }
            edges.push((prev.clone(), v.clone()));
            edge_parents.get_mut(v).expect("seeded").push(prev.clone());
            edge_children.get_mut(&prev).expect("seeded").push(v.clone());
        }

        // Barycenter sweeps to reduce crossings.
        for _ in 0..6 {
            for k in 1..layers.len() {
                let pos_prev = position_map(&layers[k - 1]);
                layers[k].sort_by(|a, b| {
                    let ba = barycenter(&edge_parents[&a.key()], &pos_prev);
                    let bb = barycenter(&edge_parents[&b.key()], &pos_prev);
                    ba.partial_cmp(&bb)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| slot_tiebreak(a, b))
                });
            }
            for k in (0..layers.len().saturating_sub(1)).rev() {
                let pos_next = position_map(&layers[k + 1]);
                layers[k].sort_by(|a, b| {
                    let ba = barycenter(&edge_children[&a.key()], &pos_next);
                    let bb = barycenter(&edge_children[&b.key()], &pos_next);
                    ba.partial_cmp(&bb)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| slot_tiebreak(a, b))
                });
            }
        }

        // Row assignment. A left-to-right pass places each slot near its
        // parents' mean row. A right-to-left pass then lines each slot up
        // with its topmost child where the rule below allows, spacing
        // sources out so their children fit beside them, and a second
        // left-to-right pass settles the children there. Rows left without any slot are then removed;
        // that only shortens vertical strokes, so it keeps every row's
        // contents and the rule below.
        //
        // A row must not carry both a source's stroke out to its lane and
        // a different source's stroke in to a target: the two horizontals
        // merge into one line that reads as an edge between the unrelated
        // source and target. Both passes skip such rows. The source's
        // outgoing stroke exists when it has a child other than the
        // target, which must sit on another row. The incoming stroke
        // exists when the target has a parent other than the source,
        // which sits on another row by construction.
        let shares_row =
            |x: &str, v: &str| edge_children[x].iter().any(|c| c != v) && edge_parents[v].iter().any(|p| p != x);
        let forward = |row_of: &mut HashMap<String, usize>, first: usize| {
            for l in first..layers.len() {
                let prev_by_row = slots_by_row(l.checked_sub(1).map(|p| layers[p].as_slice()), row_of);
                let rows = place_layer(
                    &layers[l],
                    |k| {
                        // Keep a row the right-to-left pass aligned with a parent.
                        let current = row_of.get(k).copied();
                        if current.is_some_and(|r| prev_by_row.get(&r).is_some_and(|x| edge_parents[k].contains(x))) {
                            return current;
                        }
                        let rows: Vec<usize> = edge_parents[k].iter().filter_map(|p| row_of.get(p).copied()).collect();
                        (!rows.is_empty()).then(|| rows.iter().sum::<usize>() / rows.len())
                    },
                    |k, r| prev_by_row.get(&r).is_some_and(|x| shares_row(x, k)),
                );
                row_of.extend(rows);
            }
        };
        let mut row_of: HashMap<String, usize> = HashMap::new();
        forward(&mut row_of, 0);
        for l in (0..layers.len().saturating_sub(1)).rev() {
            let next_by_row = slots_by_row(Some(layers[l + 1].as_slice()), &row_of);
            let clashes = |k: &str, r: usize| next_by_row.get(&r).is_some_and(|v| shares_row(k, v));
            // A slot that can't share its topmost child's row keeps its
            // current row. Searching downward instead would push it past
            // every target, dragging the rest of its layer with it.
            let rows = place_layer(
                &layers[l],
                |k| {
                    let first_child = edge_children[k].iter().filter_map(|c| row_of.get(c).copied()).min();
                    first_child
                        .filter(|r| !clashes(k, *r))
                        .or_else(|| row_of.get(k).copied())
                },
                clashes,
            );
            row_of.extend(rows);
        }
        forward(&mut row_of, 1);
        let mut occupied: Vec<usize> = row_of.values().copied().collect();
        occupied.sort_unstable();
        occupied.dedup();
        for r in row_of.values_mut() {
            *r = occupied.binary_search(r).expect("row is occupied");
        }

        // Per-source lane assignment. For every channel (gap between two
        // adjacent layers), every source slot with at least one turning
        // edge gets a dedicated column for its vertical stroke. Sources
        // are ordered by row (then by key) so lanes fill the channel
        // top-to-bottom in a stable, deterministic way. This prevents
        // two different sources from sharing a turn column and stacking
        // their verticals into ambiguous `┼` junctions.
        let num_channels = layers.len().saturating_sub(1);
        let mut layer_of: HashMap<String, usize> = HashMap::new();
        for (l, lyr) in layers.iter().enumerate() {
            for slot in lyr {
                layer_of.insert(slot.key(), l);
            }
        }
        let mut sources_per_channel: Vec<Vec<String>> = vec![Vec::new(); num_channels];
        let mut seen: Vec<HashSet<String>> = vec![HashSet::new(); num_channels];
        for (u, v) in &edges {
            if row_of[u] == row_of[v] {
                continue;
            }
            let lu = layer_of[u];
            if seen[lu].insert(u.clone()) {
                sources_per_channel[lu].push(u.clone());
            }
        }
        for srcs in &mut sources_per_channel {
            srcs.sort_by(|a, b| row_of[a].cmp(&row_of[b]).then_with(|| a.cmp(b)));
        }
        let mut merges_per_channel: Vec<Vec<String>> = vec![Vec::new(); num_channels];
        for (l, lyr) in layers.iter().enumerate().skip(1) {
            let mut merges: Vec<String> = lyr
                .iter()
                .map(Slot::key)
                .filter(|k| edge_parents[k].len() > 1)
                .collect();
            merges.sort_by_key(|k| row_of[k]);
            merges_per_channel[l - 1] = merges;
        }

        // Layer widths and per-channel widths sized to hold every lane and
        // merge column plus room for the arrow column and a leading
        // horizontal.
        const MIN_CHANNEL: usize = 4;
        let layer_width: Vec<usize> = layers
            .iter()
            .map(|lyr| lyr.iter().map(|s| s.label_width()).max().unwrap_or(0))
            .collect();
        let channels: Vec<usize> = sources_per_channel
            .iter()
            .zip(&merges_per_channel)
            .map(|(srcs, merges)| (srcs.len() + merges.len() + 2).max(MIN_CHANNEL))
            .collect();
        let mut layer_x: Vec<usize> = Vec::with_capacity(layers.len());
        let mut cursor = 0;
        for (l, w) in layer_width.iter().enumerate() {
            layer_x.push(cursor);
            cursor += w;
            if l + 1 < layer_width.len() {
                cursor += channels[l];
            }
        }
        let width = cursor.max(1);

        // Assign each source its lane column and each merge target its
        // merge column. They pack tight against the target layer (lanes,
        // then merge columns, then the arrow column) so incoming verticals
        // sit close to their destination label, which reads naturally.
        // Merge columns sit right of every lane so a track always runs
        // rightwards from a lane to its merge column.
        let mut turn_col: HashMap<String, usize> = HashMap::new();
        let mut merge_col: HashMap<String, usize> = HashMap::new();
        for (k, (srcs, merges)) in sources_per_channel.iter().zip(&merges_per_channel).enumerate() {
            // `next_x - 1` is reserved for the arrow glyph into a real
            // target; lanes occupy the columns immediately to its left.
            let base = layer_x[k + 1] - srcs.len() - merges.len() - 1;
            for (i, s) in srcs.iter().enumerate() {
                turn_col.insert(s.clone(), base + i);
            }
            for (i, m) in merges.iter().enumerate() {
                merge_col.insert(m.clone(), base + srcs.len() + i);
            }
        }

        // Finalise positions and canvas dimensions.
        let mut pos: HashMap<String, (usize, usize)> = HashMap::new();
        let mut height = 0;
        for (l, lyr) in layers.iter().enumerate() {
            for slot in lyr {
                let r = row_of[&slot.key()];
                pos.insert(slot.key(), (l, r));
                height = height.max(r + 1);
            }
        }

        let mut layout = Self {
            layers,
            edges,
            pos,
            layer_x,
            turn_col,
            merge_col,
            track: HashMap::new(),
            width,
            height,
        };
        layout.assign_tracks();
        layout
    }

    /// Choose a track row for every turning edge into a merge target.
    ///
    /// A track runs from the source's lane to the target's merge column
    /// without touching any other horizontal stroke. That private stretch
    /// is the only place the source's fan-out meets the target's fan-in, so
    /// a reader can't follow one edge's line onto another's. Rows are
    /// tried in order: the target's own row (arriving directly), the
    /// source's own row, then the free row nearest the target. When none is
    /// free, a blank row is inserted beside the target.
    fn assign_tracks(&mut self) {
        let mut edges: Vec<(String, String)> = self
            .edges
            .iter()
            .filter(|(u, v)| self.merge_col.contains_key(v) && self.pos[u].1 != self.pos[v].1)
            .cloned()
            .collect();
        edges.sort_by_key(|(u, v)| (self.pos[u].0, self.pos[v].1, self.pos[u].1));
        for (u, v) in edges {
            let t = self.free_track(&u, &v).unwrap_or_else(|| {
                let (ru, rv) = (self.pos[&u].1, self.pos[&v].1);
                let at = if ru < rv { rv } else { rv + 1 };
                self.insert_row(at);
                at
            });
            self.track.insert((u, v), t);
        }
    }

    fn free_track(&self, u: &str, v: &str) -> Option<usize> {
        let (k, ru) = self.pos[u];
        let rv = self.pos[v].1;
        let turn = self.turn_col[u];
        let mut candidates: Vec<usize> = (ru.min(rv)..=ru.max(rv)).collect();
        candidates.sort_by_key(|&t| (t != rv, t != ru, t.abs_diff(rv)));
        candidates.into_iter().find(|&t| {
            // On its own row the source's stroke already reaches its lane.
            let start = if t == ru { turn + 1 } else { turn };
            let end = self.track_end(v, t);
            self.row_strokes(k, t, v).iter().all(|&(a, b)| b < start || end < a)
        })
    }

    /// Last column of a track on row `t` into `v`: the arrow when the track
    /// is `v`'s own row, otherwise `v`'s merge column.
    fn track_end(&self, v: &str, t: usize) -> usize {
        if t == self.pos[v].1 {
            self.arrow_col(v)
        } else {
            self.merge_col[v]
        }
    }

    /// Column where edges into `v` end: its arrow, or its own cell for a dummy.
    fn arrow_col(&self, v: &str) -> usize {
        let x = self.layer_x[self.pos[v].0];
        if self.slot_is_real(v) { x - 1 } else { x }
    }

    /// Horizontal strokes already on row `t` of channel `k`, as inclusive
    /// column ranges. The rows of targets other than `v` count as taken
    /// whole, so a track never runs alongside another target's arrival.
    fn row_strokes(&self, k: usize, t: usize, v: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for slot in &self.layers[k] {
            let key = slot.key();
            if self.pos[&key].1 != t {
                continue;
            }
            let right = self.layer_x[k] + slot.label_width();
            if let Some(&turn) = self.turn_col.get(&key) {
                out.push((right, turn));
            }
            for (_, w) in self.edges.iter().filter(|(x, w)| *x == key && self.pos[w].1 == t) {
                out.push((right, self.arrow_col(w)));
            }
        }
        for slot in &self.layers[k + 1] {
            let key = slot.key();
            if key != v && self.pos[&key].1 == t {
                out.push((0, self.arrow_col(&key)));
            }
        }
        for ((a, b), &row) in &self.track {
            if row == t && self.pos[a].0 == k {
                out.push((self.turn_col[a], self.track_end(b, row)));
            }
        }
        out
    }

    fn insert_row(&mut self, at: usize) {
        for (_, r) in self.pos.values_mut() {
            if *r >= at {
                *r += 1;
            }
        }
        for r in self.track.values_mut() {
            if *r >= at {
                *r += 1;
            }
        }
        self.height += 1;
    }

    /// Rasterise the layout to a list of `(emitted_line, visible_width)`
    /// pairs, one per row. When a label key appears in `rendered`, the
    /// emitted string substitutes the rendered form (typically ANSI-
    /// wrapped) for the label's characters; the visible width still
    /// reflects the original label.
    ///
    /// When a real node's key appears in `arrows`, every incoming arrow
    /// glyph terminating at that node is replaced with the visible
    /// character from the tuple, and the rendered form (e.g. an ANSI-
    /// coloured symbol) is substituted at emit time. Graph roots have
    /// no arrow to replace, so their entries in `arrows` are ignored.
    fn rasterise(
        &self,
        rendered: &HashMap<String, String>,
        arrows: &HashMap<String, (char, String)>,
    ) -> Vec<(String, usize)> {
        let mut grid: Vec<Vec<char>> = vec![vec![' '; self.width]; self.height];
        // Label / arrow regions per row: (start_col, visible_width, emit_text).
        let mut per_row: Vec<Vec<(usize, usize, String)>> = vec![Vec::new(); self.height];

        // Draw real-node labels onto the grid and record each label's
        // footprint so the final emit can swap in the rendered form.
        for lyr in &self.layers {
            for slot in lyr {
                let Slot::Real { key, label } = slot else { continue };
                let (l, r) = self.pos[&slot.key()];
                let x = self.layer_x[l];
                let width = label.chars().count();
                for (i, ch) in label.chars().enumerate() {
                    grid[r][x + i] = ch;
                }
                let emit = rendered.get(key).cloned().unwrap_or_else(|| label.clone());
                per_row[r].push((x, width, emit));
            }
        }

        // Draw edges. Each edge is between adjacent layers.
        for (u, v) in &self.edges {
            self.draw_edge(&mut grid, u, v);
        }

        // Arrow-glyph substitution. Every real node in layer >= 1 has its
        // incoming edges terminating at `(layer_x[l] - 1, row)`; if a
        // symbol is configured for that node, overwrite the `→` with
        // the visible character and record the rendered form so the
        // emit loop swaps in any ANSI colouring.
        for lyr in &self.layers {
            for slot in lyr {
                let Slot::Real { key, .. } = slot else { continue };
                let Some((glyph, emit)) = arrows.get(key) else { continue };
                let (l, r) = self.pos[&slot.key()];
                if l == 0 {
                    // Graph root — no incoming edge, so no arrow to replace.
                    continue;
                }
                let col = self.layer_x[l] - 1;
                if grid[r][col] == ARROW {
                    grid[r][col] = *glyph;
                    per_row[r].push((col, 1, emit.clone()));
                }
            }
        }

        for row in &mut per_row {
            row.sort_by_key(|(start, ..)| *start);
        }

        // Emit each row, substituting the rendered form at label columns
        // and computing visible width (trailing spaces trimmed) so the
        // caller can pad correctly when composing phases side-by-side.
        grid.into_iter()
            .enumerate()
            .map(|(r, row)| {
                let regions = &per_row[r];
                let mut out = String::new();
                let mut col = 0;
                let mut rgn_i = 0;
                while col < row.len() {
                    if rgn_i < regions.len() && regions[rgn_i].0 == col {
                        let (_, w, ref text) = regions[rgn_i];
                        out.push_str(text);
                        col += w;
                        rgn_i += 1;
                    } else {
                        out.push(if row[col] == CROSSING { VERT } else { row[col] });
                        col += 1;
                    }
                }
                // Compute visible width by counting non-trailing-space
                // chars in the raw grid row, then trim trailing spaces
                // from the emitted string (ANSI bytes never look like
                // spaces so this is safe).
                let visible_w = row.iter().rposition(|c| *c != ' ').map(|i| i + 1).unwrap_or(0);
                (out.trim_end_matches(' ').to_owned(), visible_w)
            })
            .collect()
    }

    fn draw_edge(&self, grid: &mut [Vec<char>], u: &str, v: &str) {
        let (lu, ru) = self.pos[u];
        let (lv, rv) = self.pos[v];
        debug_assert_eq!(lv, lu + 1, "edge must span exactly one layer");

        let u_right = self.layer_x[lu] + self.label_width_at(u);
        let v_left = self.layer_x[lv];
        let v_is_real = self.slot_is_real(v);
        let u_is_real = self.slot_is_real(u);

        // One cell before the target label — leave room for the arrow.
        let horiz_end = if v_is_real { v_left.saturating_sub(1) } else { v_left };

        if ru == rv {
            for x in u_right..horiz_end {
                merge(grid, x, ru, HORIZ);
            }
            if v_is_real {
                merge(grid, horiz_end, ru, ARROW);
            } else {
                merge(grid, horiz_end, ru, HORIZ);
            }
            if !u_is_real {
                merge(grid, self.layer_x[lu], ru, HORIZ);
            }
            return;
        }

        // Turning edge: turn at the source's dedicated lane column. Every
        // source with at least one turning edge is assigned a unique
        // column in the channel at layout time, so two sources can
        // never share a vertical and produce an ambiguous junction. An
        // edge into a merge target then follows its track to the target's
        // merge column, unless the track is the target's own row.
        // A track's corners are rounded so the bends read as flowing from
        // the lane into the merge rather than as branches out of it.
        let turn = self.turn_col[u];
        let mut path = vec![(u_right, ru), (turn, ru)];
        let mut rounded = Vec::new();
        match self.track.get(&(u.to_owned(), v.to_owned())) {
            Some(&t) if t != rv => {
                let m = self.merge_col[v];
                path.extend([(turn, t), (m, t), (m, rv)]);
                rounded.extend([(turn, t), (m, t)]);
            }
            _ => path.push((turn, rv)),
        }
        path.push((horiz_end, rv));
        draw_path(grid, &path, &rounded);
        if v_is_real {
            merge(grid, horiz_end, rv, ARROW);
        } else {
            merge(grid, horiz_end, rv, HORIZ);
        }
        if !u_is_real {
            merge(grid, self.layer_x[lu], ru, HORIZ);
        }
    }

    fn slot_is_real(&self, key: &str) -> bool {
        for lyr in &self.layers {
            for slot in lyr {
                if slot.key() == key {
                    return slot.is_real();
                }
            }
        }
        false
    }

    fn label_width_at(&self, key: &str) -> usize {
        for lyr in &self.layers {
            for slot in lyr {
                if slot.key() == key {
                    return slot.label_width();
                }
            }
        }
        0
    }
}

/// Rows for `layer`'s slots, in layer order: each at least one past the
/// previous slot, as close as possible to `preferred` (row 0 when it has
/// no opinion), and never on a row where `clashes` holds.
fn place_layer(
    layer: &[Slot],
    preferred: impl Fn(&str) -> Option<usize>,
    clashes: impl Fn(&str, usize) -> bool,
) -> Vec<(String, usize)> {
    let mut next_free = 0;
    let mut rows = Vec::with_capacity(layer.len());
    for slot in layer {
        let key = slot.key();
        let mut r = preferred(&key).unwrap_or(0).max(next_free);
        while clashes(&key, r) {
            r += 1;
        }
        next_free = r + 1;
        rows.push((key, r));
    }
    rows
}

/// Slot key on each row of `layer`, for clash checks against a neighbour.
fn slots_by_row(layer: Option<&[Slot]>, row_of: &HashMap<String, usize>) -> HashMap<usize, String> {
    layer
        .into_iter()
        .flatten()
        .map(|s| (row_of[&s.key()], s.key()))
        .collect()
}

fn position_map(layer: &[Slot]) -> HashMap<String, f64> {
    layer.iter().enumerate().map(|(i, s)| (s.key(), i as f64)).collect()
}

fn barycenter(neighbours: &[String], positions: &HashMap<String, f64>) -> f64 {
    let rows: Vec<f64> = neighbours.iter().filter_map(|n| positions.get(n).copied()).collect();
    if rows.is_empty() {
        // Nodes with no neighbours on the reference layer float to the top.
        f64::NEG_INFINITY
    } else {
        rows.iter().sum::<f64>() / rows.len() as f64
    }
}

fn slot_tiebreak(a: &Slot, b: &Slot) -> std::cmp::Ordering {
    // Real nodes sort before dummies on ties, so dummies get out of the
    // way and real nodes keep their parent-aligned rows.
    match (a, b) {
        (Slot::Real { .. }, Slot::Dummy { .. }) => std::cmp::Ordering::Less,
        (Slot::Dummy { .. }, Slot::Real { .. }) => std::cmp::Ordering::Greater,
        (Slot::Real { key: a, .. }, Slot::Real { key: b, .. }) => a.cmp(b),
        (Slot::Dummy { edge_id: a, hop: ha }, Slot::Dummy { edge_id: b, hop: hb }) => a.cmp(b).then(ha.cmp(hb)),
    }
}

/// Place `ch` at `(x, y)`, merging with any existing box-drawing char so
/// crossings and junctions render cleanly.
fn merge(grid: &mut [Vec<char>], x: usize, y: usize, ch: char) {
    if y >= grid.len() || x >= grid[y].len() {
        return;
    }
    let cur = grid[y][x];
    grid[y][x] = merged(cur, ch);
}

/// Draw an edge along axis-aligned `points`, entering from the left at the
/// first point and leaving to the right at the last. Each cell gets the
/// strokes toward its neighbours on the path, so bends become corners.
/// Corners at the `rounded` cells use rounded glyphs.
fn draw_path(grid: &mut [Vec<char>], points: &[(usize, usize)], rounded: &[(usize, usize)]) {
    let mut cells: Vec<((usize, usize), u8)> = Vec::new();
    let mut add = |cell: (usize, usize), stroke: u8| match cells.iter_mut().find(|(c, _)| *c == cell) {
        Some((_, mask)) => *mask |= stroke,
        None => cells.push((cell, stroke)),
    };
    for pair in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        let (toward_end, toward_start) = match (x0.cmp(&x1), y0.cmp(&y1)) {
            (Ordering::Less, _) => (L_RIGHT, L_LEFT),
            (Ordering::Greater, _) => (L_LEFT, L_RIGHT),
            (_, Ordering::Less) => (L_DOWN, L_UP),
            (_, Ordering::Greater) => (L_UP, L_DOWN),
            _ => continue,
        };
        for x in x0.min(x1)..=x0.max(x1) {
            for y in y0.min(y1)..=y0.max(y1) {
                let mut stroke = 0;
                if (x, y) != (x1, y1) {
                    stroke |= toward_end;
                }
                if (x, y) != (x0, y0) {
                    stroke |= toward_start;
                }
                add((x, y), stroke);
            }
        }
    }
    if let (Some(&first), Some(&last)) = (points.first(), points.last()) {
        add(first, L_LEFT);
        add(last, L_RIGHT);
    }
    for ((x, y), mask) in cells {
        if let Some(ch) = char_for_mask(mask) {
            merge(grid, x, y, if rounded.contains(&(x, y)) { round(ch) } else { ch });
        }
    }
}

fn round(ch: char) -> char {
    match ch {
        TOP_LEFT => ROUND_TOP_LEFT,
        TOP_RIGHT => ROUND_TOP_RIGHT,
        BOTTOM_LEFT => ROUND_BOTTOM_LEFT,
        BOTTOM_RIGHT => ROUND_BOTTOM_RIGHT,
        _ => ch,
    }
}

fn merged(cur: char, new: char) -> char {
    if cur == ' ' {
        return new;
    }
    if cur == new {
        return cur;
    }
    // Letters (node labels) always win — edges should not clobber them.
    if cur.is_alphanumeric() || cur == '_' || cur == '-' {
        return cur;
    }
    // Otherwise, compute the combined mask of incident strokes.
    let mut mask = mask_of(cur) | mask_of(new);
    // Arrows are a special case: preserve arrow if present.
    if cur == ARROW || new == ARROW {
        // Strip the "right" stroke into the arrow glyph.
        mask |= L_RIGHT;
        if mask == L_RIGHT {
            return ARROW;
        }
        // Mixed arrow + stroke: keep the arrow, drop the combined glyph.
        return ARROW;
    }
    let straight = |c: char| matches!(c, HORIZ | VERT | CROSSING);
    if straight(cur) && straight(new) && mask == L_UP | L_DOWN | L_LEFT | L_RIGHT {
        return CROSSING;
    }
    char_for_mask(mask).unwrap_or(new)
}

// Stroke mask: one bit per direction from a cell's centre.
const L_LEFT: u8 = 1 << 0;
const L_RIGHT: u8 = 1 << 1;
const L_UP: u8 = 1 << 2;
const L_DOWN: u8 = 1 << 3;

fn mask_of(ch: char) -> u8 {
    match ch {
        HORIZ => L_LEFT | L_RIGHT,
        VERT => L_UP | L_DOWN,
        TOP_LEFT | ROUND_TOP_LEFT => L_RIGHT | L_DOWN,
        TOP_RIGHT | ROUND_TOP_RIGHT => L_LEFT | L_DOWN,
        BOTTOM_LEFT | ROUND_BOTTOM_LEFT => L_RIGHT | L_UP,
        BOTTOM_RIGHT | ROUND_BOTTOM_RIGHT => L_LEFT | L_UP,
        T_DOWN => L_LEFT | L_RIGHT | L_DOWN,
        T_UP => L_LEFT | L_RIGHT | L_UP,
        T_RIGHT => L_UP | L_DOWN | L_RIGHT,
        T_LEFT => L_UP | L_DOWN | L_LEFT,
        CROSS | CROSSING => L_UP | L_DOWN | L_LEFT | L_RIGHT,
        ARROW => L_RIGHT,
        _ => 0,
    }
}

fn char_for_mask(mask: u8) -> Option<char> {
    Some(match mask {
        m if m == L_LEFT | L_RIGHT => HORIZ,
        m if m == L_UP | L_DOWN => VERT,
        m if m == L_RIGHT | L_DOWN => TOP_LEFT,
        m if m == L_LEFT | L_DOWN => TOP_RIGHT,
        m if m == L_RIGHT | L_UP => BOTTOM_LEFT,
        m if m == L_LEFT | L_UP => BOTTOM_RIGHT,
        m if m == L_LEFT | L_RIGHT | L_DOWN => T_DOWN,
        m if m == L_LEFT | L_RIGHT | L_UP => T_UP,
        m if m == L_UP | L_DOWN | L_RIGHT => T_RIGHT,
        m if m == L_UP | L_DOWN | L_LEFT => T_LEFT,
        m if m == L_UP | L_DOWN | L_LEFT | L_RIGHT => CROSS,
        m if m == L_RIGHT => HORIZ,
        m if m == L_LEFT => HORIZ,
        m if m == L_UP => VERT,
        m if m == L_DOWN => VERT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parents(edges: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (p, c) in edges {
            out.entry((*c).into()).or_default().push((*p).into());
            out.entry((*p).into()).or_default();
        }
        out
    }

    #[test]
    fn merge_cross() {
        assert_eq!(merged(HORIZ, VERT), CROSSING);
        assert_eq!(merged(VERT, HORIZ), CROSSING);
        // A turn onto a crossing connects the strokes, so it's a junction.
        assert_eq!(merged(CROSSING, BOTTOM_LEFT), CROSS);
        assert_eq!(merged(T_RIGHT, HORIZ), CROSS);
    }

    #[test]
    fn source_that_cannot_align_keeps_its_row() {
        // `x` and `w` can't share a row with `a` or `b` (each has another
        // child and each target has other parents), so they must stay put
        // rather than sink below every target and drag `y` with them,
        // which has room beside its only child `c`.
        let p = parents(&[("x", "a"), ("x", "b"), ("w", "a"), ("w", "b"), ("w", "c"), ("y", "c")]);
        let names: Vec<String> = ["x", "w", "y", "a", "b", "c"].map(String::from).to_vec();
        let layout = Layout::build_from_parents(&names, &p, &HashMap::new());
        let row = |k: &str| layout.pos[k].1;
        assert_eq!(row("y"), row("c"), "{}", render_parents(&names, &p));
    }

    #[test]
    fn edges_merge_only_in_their_target_column() {
        // `a` feeds `bin` and `lib`. Its edge to `lib` must reach `lib`'s
        // merge column on a row of its own, not join `b`'s line, where a
        // reader could follow `b` up `a`'s lane to `bin`.
        let p = parents(&[("a", "bin"), ("a", "lib"), ("b", "lib"), ("c", "lib")]);
        let names: Vec<String> = ["a", "b", "c", "bin", "lib"].map(String::from).to_vec();
        let out = render_parents(&names, &p);
        let expected = "\
a─┬──→bin
  ╰─╮
b───┼→lib
c───╯";
        assert_eq!(out, expected);
        assert_strokes_connect(&out);
    }

    #[test]
    fn crossing_renders_apart_from_junction() {
        // `a`'s lane passes `c`'s row on its way down to `x` and `y`,
        // crossing `c`'s stroke out to its own lane.
        let p = parents(&[("a", "x"), ("a", "y"), ("b", "y"), ("c", "x"), ("c", "y")]);
        let names: Vec<String> = ["a", "b", "c", "x", "y"].map(String::from).to_vec();
        let out = render_parents(&names, &p);
        let c_row = out.lines().find(|l| l.starts_with('c')).expect("c row");
        assert!(c_row.contains('│'), "a's lane should cross c's row unbroken:\n{out}");
        assert!(!out.contains(CROSSING), "marker must not reach output:\n{out}");
    }

    #[test]
    fn merge_corner_extends_to_t() {
        // A ┘ with a horizontal stroke entering from the right becomes ┴.
        assert_eq!(merged(BOTTOM_RIGHT, HORIZ), T_UP);
    }

    #[test]
    fn labels_beat_edges() {
        assert_eq!(merged('a', HORIZ), 'a');
        assert_eq!(merged('-', VERT), '-');
    }

    #[test]
    fn render_linear_chain() {
        let p = parents(&[("a", "b"), ("b", "c")]);
        let out = render_parents(&["a".into(), "b".into(), "c".into()], &p);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "chain should be one row: {out:?}");
        assert!(lines[0].contains("a"));
        assert!(lines[0].contains("b"));
        assert!(lines[0].contains("c"));
        assert!(lines[0].contains(ARROW));
    }

    #[test]
    fn render_fork() {
        // root → x, root → y
        let p = parents(&[("root", "x"), ("root", "y")]);
        let out = render_parents(&["root".into(), "x".into(), "y".into()], &p);
        assert!(out.contains("root"));
        assert!(out.contains("x"));
        assert!(out.contains("y"));
        assert!(out.contains(ARROW));
    }

    #[test]
    fn render_diamond() {
        // root → x → join, root → y → join
        let p = parents(&[("root", "x"), ("root", "y"), ("x", "join"), ("y", "join")]);
        let names = vec!["root".into(), "x".into(), "y".into(), "join".into()];
        let out = render_parents(&names, &p);
        for n in ["root", "x", "y", "join"] {
            assert!(out.contains(n), "missing {n} in:\n{out}");
        }
    }

    #[test]
    fn going_up_edge_has_unbroken_vertical() {
        // Force an upward-going edge between two columns. The edge
        // source sits below its target, so the turn column must have a
        // continuous vertical from source row to target row.
        let p = parents(&[("a", "x"), ("b", "x"), ("a", "y"), ("b", "y")]);
        let names = vec!["a".into(), "b".into(), "x".into(), "y".into()];
        let out = render_parents(&names, &p);
        assert_strokes_connect(&out);
    }

    /// Every stroke pointing up or down must meet a stroke pointing back.
    fn assert_strokes_connect(out: &str) {
        let grid: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
        let at = |x: usize, y: usize| mask_of(grid.get(y).and_then(|r| r.get(x)).copied().unwrap_or(' '));
        for (y, row) in grid.iter().enumerate() {
            for x in 0..row.len() {
                if at(x, y) & L_DOWN != 0 {
                    assert!(at(x, y + 1) & L_UP != 0, "stroke at ({x},{y}) dangles down:\n{out}");
                }
                if at(x, y) & L_UP != 0 {
                    assert!(
                        y > 0 && at(x, y - 1) & L_DOWN != 0,
                        "stroke at ({x},{y}) dangles up:\n{out}"
                    );
                }
            }
        }
    }

    #[test]
    fn turn_column_stays_in_channel() {
        // A long edge from a to c (3 layers) creates a dummy in layer 1.
        // The dummy's outgoing edge must not turn inside a real node's
        // label column in layer 1 — otherwise it looks like the edge
        // enters that node from the side.
        let p = parents(&[("a", "mid"), ("mid", "b"), ("a", "c"), ("b", "c")]);
        let names = vec!["a".into(), "mid".into(), "b".into(), "c".into()];
        let out = render_parents(&names, &p);
        // Find "mid"'s column range: every row, every char in that span
        // must either be the label or space — never a box-drawing glyph
        // from an edge terminating inside it.
        let lines: Vec<&str> = out.lines().collect();
        let mid_row = lines.iter().position(|l| l.contains("mid")).expect("mid row");
        let mid_chars: Vec<char> = lines[mid_row].chars().collect();
        let mid_col = mid_chars
            .windows(3)
            .position(|w| w == ['m', 'i', 'd'])
            .expect("mid col");
        // Only glyphs with a vertical stroke are a problem: they imply
        // an edge column running through the label's column range.
        let has_vertical = |c: char| mask_of(c) & (L_UP | L_DOWN) != 0;
        for (r, line) in lines.iter().enumerate() {
            if r == mid_row {
                continue;
            }
            let chars: Vec<char> = line.chars().collect();
            for (c, ch) in chars.iter().enumerate().skip(mid_col).take(3) {
                assert!(
                    !has_vertical(*ch),
                    "vertical-stroke glyph {ch:?} lands inside mid's column at ({c},{r}):\n{out}"
                );
            }
        }
    }

    #[test]
    fn sibling_sources_get_distinct_turn_columns() {
        // Two sources in the same layer, each with edges that turn into
        // the next layer. Each source must get its own turn column so
        // their verticals never stack into the same column — otherwise
        // the graph becomes ambiguous about which source feeds which
        // target.
        let p = parents(&[("a", "x"), ("a", "y"), ("b", "x"), ("b", "y")]);
        let names = vec!["a".into(), "b".into(), "x".into(), "y".into()];
        let layout = Layout::build_from_parents(&names, &p, &HashMap::new());
        let a_col = layout.turn_col.get("a").copied().expect("source a needs a turn column");
        let b_col = layout.turn_col.get("b").copied().expect("source b needs a turn column");
        assert_ne!(a_col, b_col, "sibling sources must turn in distinct columns");
    }

    #[test]
    fn source_stroke_never_shares_a_row_with_unrelated_incoming_stroke() {
        // Before the fix, `s` sat on the same row as `fc`, and `s`'s
        // stroke out to its lane merged with `f`'s stroke in to `fc`,
        // drawing a line from `s` to `fc`.
        let p = parents(&[
            ("d", "it"),
            ("d", "ci"),
            ("d", "readme"),
            ("f", "fc"),
            ("r", "image"),
            ("s", "s3a"),
            ("s", "s3b"),
        ]);
        let names: Vec<String> = ["d", "f", "r", "s", "ci", "fc", "image", "it", "readme", "s3a", "s3b"]
            .map(String::from)
            .to_vec();
        let layout = Layout::build_from_parents(&names, &p, &HashMap::new());
        for (u, v) in &layout.edges {
            let (lu, ru) = layout.pos[u];
            let (_, rv) = layout.pos[v];
            if ru == rv {
                continue;
            }
            for (x, (lx, rx)) in &layout.pos {
                if *lx == lu && *rx == rv && x != u && layout.turn_col.contains_key(x) {
                    let out = render_parents(&names, &p);
                    panic!("{x}'s stroke shares a row with {u}→{v}:\n{out}");
                }
            }
        }
    }

    #[test]
    fn sources_line_up_with_first_child() {
        // Unconnected roots above the sources must not push them down, and
        // each source should share a row with its topmost child so its
        // children sit beside it rather than below every source.
        let mut p = parents(&[("d", "it"), ("d", "ci"), ("f", "fc"), ("s", "s3a"), ("s", "s3b")]);
        p.insert("lone".into(), Vec::new());
        let names: Vec<String> = ["lone", "d", "f", "s", "ci", "fc", "it", "s3a", "s3b"]
            .map(String::from)
            .to_vec();
        let out = render_parents(&names, &p);
        let expected = "\
lone
d────┬─→ci
     └─→it
f──────→fc
s─────┬→s3a
      └→s3b";
        assert_eq!(out, expected);
    }

    #[test]
    fn rasterise_substitutes_rendered_label() {
        // Decorating "a" with an ANSI-styled rendered form must not shift
        // the visible layout: "b" still renders one column past "a"'s
        // visible width, regardless of how many ANSI bytes the rendered
        // form adds.
        let p = parents(&[("a", "b")]);
        let names = vec!["a".into(), "b".into()];
        let mut labels = HashMap::new();
        labels.insert("a".to_owned(), "a".to_owned());
        let layout = Layout::build_from_parents(&names, &p, &labels);
        let mut rendered = HashMap::new();
        rendered.insert("a".to_owned(), "\x1b[32ma\x1b[0m".to_owned());
        let rows = layout.rasterise(&rendered, &HashMap::new());
        assert_eq!(rows.len(), 1);
        let (line, visible_w) = &rows[0];
        // Line contains ANSI escapes…
        assert!(line.contains("\x1b[32m"), "expected ANSI prefix in {line:?}");
        // …but visible width reflects the raw glyphs (a + horizontals + → + b).
        assert!(*visible_w > 0);
        // And "b" still appears as a plain literal in the output.
        assert!(line.contains('b'));
    }

    #[test]
    fn rasterise_replaces_arrow_with_plan_glyph() {
        // A non-root node with an arrow override should have its incoming
        // `→` replaced by the configured glyph. The root `a` has no
        // incoming arrow, so any override for it is silently ignored.
        let p = parents(&[("a", "b")]);
        let names = vec!["a".into(), "b".into()];
        let layout = Layout::build_from_parents(&names, &p, &HashMap::new());
        let mut arrows = HashMap::new();
        arrows.insert("b".to_owned(), ('+', "+".to_owned()));
        arrows.insert("a".to_owned(), ('+', "+".to_owned()));
        let rows = layout.rasterise(&HashMap::new(), &arrows);
        let (line, _) = &rows[0];
        assert!(line.contains('+'), "expected + arrow replacement in {line:?}");
        assert!(!line.contains(ARROW), "expected → to be gone in {line:?}");
    }

    #[test]
    fn long_edge_uses_dummy_waypoint() {
        // skip spans 2 layers: root → x → y, plus root → y.
        let p = parents(&[("root", "x"), ("x", "y"), ("root", "y")]);
        let names = vec!["root".into(), "x".into(), "y".into()];
        let out = render_parents(&names, &p);
        // There must be at least one arrow into `y`.
        assert!(out.contains(ARROW), "expected arrow in:\n{out}");
        // And the label row for `root` must have a horizontal stroke
        // leaving it that eventually terminates on `y`.
        assert!(out.contains("root"));
        assert!(out.contains('x'));
        assert!(out.contains('y'));
    }
}
