//! Left-to-right ASCII DAG visualisation.
//!
//! Nodes are laid out in columns by longest-path layer. Within each
//! column, nodes are ordered by a barycenter heuristic to reduce edge
//! crossings. Long edges (spanning more than one layer) are broken by
//! dummy waypoints so every rendered edge sits between adjacent
//! columns. The canvas is then rasterised with Unicode box-drawing
//! characters.

use std::collections::{HashMap, HashSet};

use crate::ast::Phase;
use crate::dag::Dag;

const HORIZ: char = '─';
const VERT: char = '│';
const TOP_LEFT: char = '┌';
const TOP_RIGHT: char = '┐';
const BOTTOM_LEFT: char = '└';
const BOTTOM_RIGHT: char = '┘';
const T_DOWN: char = '┬';
const T_UP: char = '┴';
const T_RIGHT: char = '├';
const T_LEFT: char = '┤';
const CROSS: char = '┼';
const ARROW: char = '→';

/// Render the DAG as a left-to-right ASCII graph, grouped by phase.
///
/// Blocks are partitioned into pre / default / post phases, and each
/// phase is rendered as its own subgraph laid out side-by-side from
/// left to right. Edges between phases are omitted — the horizontal
/// layout implies ordering. Within a phase, all parent edges are
/// rendered (both content and ordering), since the graph represents
/// run-time relationships.
pub fn render(dag: &Dag, names: &[String]) -> String {
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

    let parts: Vec<Vec<Vec<char>>> = [pre, default, post]
        .iter()
        .filter(|phase| !phase.is_empty())
        .map(|phase| render_phase(dag, phase).lines().map(|l| l.chars().collect()).collect())
        .collect();

    let height = parts.iter().map(|p| p.len()).max().unwrap_or(0);
    let widths: Vec<usize> = parts
        .iter()
        .map(|p| p.iter().map(|r| r.len()).max().unwrap_or(0))
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
            let line: &[char] = part.get(row).map(|l| l.as_slice()).unwrap_or(&[]);
            for c in line {
                out.push(*c);
            }
            if i + 1 < parts.len() {
                for _ in line.len()..widths[i] {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

fn render_phase(dag: &Dag, names: &[String]) -> String {
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
    render_parents(names, &parents)
}

/// Render from a precomputed parents map. `topo` must list every node in
/// topological order (parents before children). Exposed for tests that
/// can't easily construct a full `Dag`.
fn render_parents(topo: &[String], parents: &HashMap<String, Vec<String>>) -> String {
    Layout::build_from_parents(topo, parents).rasterise()
}

/// A layout slot. Real nodes carry their block name; dummies are
/// waypoints inserted so long edges traverse adjacent columns only.
/// `edge_id` identifies the originating edge; `hop` distinguishes
/// multiple dummies along the same edge.
#[derive(Clone, Debug)]
enum Slot {
    Real(String),
    Dummy { edge_id: usize, hop: usize },
}

impl Slot {
    fn key(&self) -> String {
        match self {
            Slot::Real(n) => n.clone(),
            Slot::Dummy { edge_id, hop } => format!("__dummy_{edge_id}_{hop}"),
        }
    }

    fn is_real(&self) -> bool {
        matches!(self, Slot::Real(_))
    }

    fn label_width(&self) -> usize {
        match self {
            Slot::Real(n) => n.chars().count(),
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
    /// Total grid width and height.
    width: usize,
    height: usize,
}

impl Layout {
    fn build_from_parents(names: &[String], parents: &HashMap<String, Vec<String>>) -> Self {
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

        // Seed each layer with real nodes, alphabetically.
        let mut layers: Vec<Vec<Slot>> = vec![Vec::new(); max_layer + 1];
        let mut sorted = names.to_vec();
        sorted.sort();
        for name in &sorted {
            layers[layer[name]].push(Slot::Real(name.clone()));
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

        // Row assignment: walk layers left-to-right, placing each slot on
        // a row at least one past the previous slot in its layer, and as
        // close as possible to its parents' mean row.
        let mut row_of: HashMap<String, usize> = HashMap::new();
        for lyr in &layers {
            let mut used: HashSet<usize> = HashSet::new();
            let mut floor: i64 = -1;
            for slot in lyr {
                let ps = &edge_parents[&slot.key()];
                let preferred: usize = {
                    let rows: Vec<usize> = ps.iter().filter_map(|p| row_of.get(p).copied()).collect();
                    if rows.is_empty() {
                        0
                    } else {
                        rows.iter().sum::<usize>() / rows.len()
                    }
                };
                let mut r = preferred.max((floor + 1).max(0) as usize);
                while used.contains(&r) {
                    r += 1;
                }
                used.insert(r);
                row_of.insert(slot.key(), r);
                floor = r as i64;
            }
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

        // Layer widths and per-channel widths sized to hold every lane
        // plus room for the arrow column and a leading horizontal.
        const MIN_CHANNEL: usize = 4;
        let layer_width: Vec<usize> = layers
            .iter()
            .map(|lyr| lyr.iter().map(|s| s.label_width()).max().unwrap_or(0))
            .collect();
        let channels: Vec<usize> = sources_per_channel
            .iter()
            .map(|srcs| (srcs.len() + 2).max(MIN_CHANNEL))
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

        // Assign each source its lane column. Lanes pack tight against
        // the target layer (rightmost N+1 columns are: lane_0 … lane_{N-1}
        // then the arrow column) so incoming verticals sit close to
        // their destination label, which reads naturally.
        let mut turn_col: HashMap<String, usize> = HashMap::new();
        for (k, srcs) in sources_per_channel.iter().enumerate() {
            if srcs.is_empty() {
                continue;
            }
            // `next_x - 1` is reserved for the arrow glyph into a real
            // target; lanes occupy the columns immediately to its left.
            let base = layer_x[k + 1] - srcs.len() - 1;
            for (i, s) in srcs.iter().enumerate() {
                turn_col.insert(s.clone(), base + i);
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

        Self {
            layers,
            edges,
            pos,
            layer_x,
            turn_col,
            width,
            height,
        }
    }

    fn rasterise(&self) -> String {
        let mut grid: Vec<Vec<char>> = vec![vec![' '; self.width]; self.height];

        // Draw real-node labels.
        for lyr in &self.layers {
            for slot in lyr {
                let Slot::Real(name) = slot else { continue };
                let (l, r) = self.pos[&slot.key()];
                let x = self.layer_x[l];
                for (i, ch) in name.chars().enumerate() {
                    grid[r][x + i] = ch;
                }
            }
        }

        // Draw edges. Each edge is between adjacent layers.
        for (u, v) in &self.edges {
            self.draw_edge(&mut grid, u, v);
        }

        grid.into_iter()
            .map(|row| {
                let s: String = row.into_iter().collect();
                s.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
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
        // never share a vertical and produce an ambiguous junction.
        let turn = self.turn_col[u];

        for x in u_right..turn {
            merge(grid, x, ru, HORIZ);
        }
        merge(grid, turn, ru, if rv > ru { TOP_RIGHT } else { BOTTOM_RIGHT });
        let (lo, hi) = (ru.min(rv) + 1, ru.max(rv).saturating_sub(1));
        for y in lo..=hi {
            merge(grid, turn, y, VERT);
        }
        merge(grid, turn, rv, if rv > ru { BOTTOM_LEFT } else { TOP_LEFT });
        for x in (turn + 1)..horiz_end {
            merge(grid, x, rv, HORIZ);
        }
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
        (Slot::Real(_), Slot::Dummy { .. }) => std::cmp::Ordering::Less,
        (Slot::Dummy { .. }, Slot::Real(_)) => std::cmp::Ordering::Greater,
        (Slot::Real(n1), Slot::Real(n2)) => n1.cmp(n2),
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
        TOP_LEFT => L_RIGHT | L_DOWN,
        TOP_RIGHT => L_LEFT | L_DOWN,
        BOTTOM_LEFT => L_RIGHT | L_UP,
        BOTTOM_RIGHT => L_LEFT | L_UP,
        T_DOWN => L_LEFT | L_RIGHT | L_DOWN,
        T_UP => L_LEFT | L_RIGHT | L_UP,
        T_RIGHT => L_UP | L_DOWN | L_RIGHT,
        T_LEFT => L_UP | L_DOWN | L_LEFT,
        CROSS => L_UP | L_DOWN | L_LEFT | L_RIGHT,
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
        assert_eq!(merged(HORIZ, VERT), CROSS);
        assert_eq!(merged(VERT, HORIZ), CROSS);
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
        let grid: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
        for x in 0..grid.iter().map(|r| r.len()).max().unwrap_or(0) {
            let col: Vec<char> = grid.iter().map(|r| r.get(x).copied().unwrap_or(' ')).collect();
            // Find the topmost and bottommost non-space cells in this
            // column; everything between must be non-space.
            let Some(top) = col.iter().position(|c| *c != ' ') else {
                continue;
            };
            let Some(bot) = col.iter().rposition(|c| *c != ' ') else {
                continue;
            };
            for (y, c) in col.iter().enumerate().take(bot).skip(top + 1) {
                assert!(*c != ' ', "column {x} has gap at row {y}:\n{out}");
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
        let has_vertical = |c: char| matches!(c, '│' | '└' | '┘' | '┌' | '┐' | '├' | '┤' | '┴' | '┬' | '┼');
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
        let layout = Layout::build_from_parents(&names, &p);
        let a_col = layout.turn_col.get("a").copied().expect("source a needs a turn column");
        let b_col = layout.turn_col.get("b").copied().expect("source b needs a turn column");
        assert_ne!(a_col, b_col, "sibling sources must turn in distinct columns");
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
