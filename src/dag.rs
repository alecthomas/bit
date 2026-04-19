use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use petgraph::Direction;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::ast::{Expr, Field, Phase, StringPart};
use crate::provider::DynResource;

/// The type of edge between two blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EdgeKind {
    /// Content-coupled: parent state is part of child's content hash.
    Dependency,
    /// Ordering-only: parent runs first, but doesn't affect child's hash.
    Ordering,
}

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("duplicate block name: {0}")]
    DuplicateBlock(String),
    #[error("unknown block: {0}")]
    UnknownBlock(String),
    #[error("dependency cycle detected")]
    Cycle,
    #[error("target '{0}' references unknown block '{1}'")]
    UnknownTargetBlock(String, String),
}

/// A node in the dependency graph.
pub struct DagNode {
    pub pos: crate::ast::Pos,
    pub name: String,
    pub doc: Option<String>,
    pub phase: Phase,
    pub provider: String,
    pub resource_name: String,
    pub protected: bool,
    /// Raw field expressions — evaluated at execution time when upstream
    /// outputs are available.
    pub fields: Vec<Field>,
    /// The resource implementation that handles resolve/plan/apply/destroy.
    pub resource: Box<dyn DynResource>,
    /// Prior state loaded from the state store (if any).
    pub prior_state: Option<serde_json::Value>,
}

/// A named target with optional documentation.
pub struct DagTarget {
    pub blocks: Vec<String>,
    pub doc: Option<String>,
}

/// The dependency graph of blocks, ready for the engine.
pub struct Dag {
    graph: DiGraph<DagNode, EdgeKind>,
    indices: HashMap<String, NodeIndex>,
    targets: HashMap<String, DagTarget>,
}

impl Dag {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            indices: HashMap::new(),
            targets: HashMap::new(),
        }
    }

    /// Add a block node to the graph.
    pub fn add_node(&mut self, node: DagNode) -> Result<(), DagError> {
        if self.indices.contains_key(&node.name) {
            return Err(DagError::DuplicateBlock(node.name.clone()));
        }
        let name = node.name.clone();
        let idx = self.graph.add_node(node);
        self.indices.insert(name, idx);
        Ok(())
    }

    /// Add a dependency edge: `from` must complete before `to`, and
    /// `from`'s state is included in `to`'s content hash.
    pub fn add_dep_edge(&mut self, from: &str, to: &str) -> Result<(), DagError> {
        self.add_edge(from, to, EdgeKind::Dependency)
    }

    /// Add an ordering edge: `from` must complete before `to`, but
    /// `from`'s state does not affect `to`'s content hash.
    pub fn add_ordering_edge(&mut self, from: &str, to: &str) -> Result<(), DagError> {
        self.add_edge(from, to, EdgeKind::Ordering)
    }

    fn add_edge(&mut self, from: &str, to: &str, kind: EdgeKind) -> Result<(), DagError> {
        let from_idx = self
            .indices
            .get(from)
            .ok_or_else(|| DagError::UnknownBlock(from.into()))?;
        let to_idx = self.indices.get(to).ok_or_else(|| DagError::UnknownBlock(to.into()))?;
        self.graph.add_edge(*from_idx, *to_idx, kind);
        Ok(())
    }

    /// Register a target.
    pub fn add_target(&mut self, name: String, blocks: Vec<String>, doc: Option<String>) {
        self.targets.insert(name, DagTarget { blocks, doc });
    }

    /// Add synthetic ordering edges between phases.
    /// Every default block gets an ordering edge from each pre block,
    /// and every post block gets an ordering edge from each default block.
    pub fn wire_phase_edges(&mut self) {
        let mut pre = Vec::new();
        let mut default = Vec::new();
        let mut post = Vec::new();
        for (name, idx) in &self.indices {
            match self.graph[*idx].phase {
                Phase::Pre => pre.push(name.clone()),
                Phase::Default => default.push(name.clone()),
                Phase::Post => post.push(name.clone()),
            }
        }
        for p in &pre {
            for d in &default {
                let from = self.indices[p];
                let to = self.indices[d];
                self.graph.add_edge(from, to, EdgeKind::Ordering);
            }
            for q in &post {
                let from = self.indices[p];
                let to = self.indices[q];
                self.graph.add_edge(from, to, EdgeKind::Ordering);
            }
        }
        for d in &default {
            for q in &post {
                let from = self.indices[d];
                let to = self.indices[q];
                self.graph.add_edge(from, to, EdgeKind::Ordering);
            }
        }
    }

    /// Validate the graph: no cycles, all target references are valid.
    pub fn validate(&self) -> Result<(), DagError> {
        toposort(&self.graph, None).map_err(|_| DagError::Cycle)?;
        for (name, target) in &self.targets {
            for block in &target.blocks {
                let block_name = block.split('.').next().unwrap_or(block);
                if !self.indices.contains_key(block_name) {
                    return Err(DagError::UnknownTargetBlock(name.clone(), block_name.to_owned()));
                }
            }
        }
        Ok(())
    }

    /// Return block names in topological order (dependencies first).
    /// Ties between independent nodes are broken alphabetically on block
    /// name, so the order is deterministic across runs.
    pub fn topo_order(&self) -> Result<Vec<String>, DagError> {
        let mut in_degree: HashMap<NodeIndex, usize> = HashMap::with_capacity(self.graph.node_count());
        for idx in self.graph.node_indices() {
            in_degree.insert(idx, self.graph.neighbors_directed(idx, Direction::Incoming).count());
        }

        // Min-heap keyed on block name for deterministic tie-breaking.
        let mut ready: BinaryHeap<Reverse<(String, NodeIndex)>> = BinaryHeap::new();
        for (&idx, &deg) in &in_degree {
            if deg == 0 {
                ready.push(Reverse((self.graph[idx].name.clone(), idx)));
            }
        }

        let mut out = Vec::with_capacity(in_degree.len());
        while let Some(Reverse((_, idx))) = ready.pop() {
            out.push(self.graph[idx].name.clone());
            for nbr in self.graph.neighbors_directed(idx, Direction::Outgoing) {
                let Some(deg) = in_degree.get_mut(&nbr) else { continue };
                *deg -= 1;
                if *deg == 0 {
                    ready.push(Reverse((self.graph[nbr].name.clone(), nbr)));
                }
            }
        }

        if out.len() != self.graph.node_count() {
            return Err(DagError::Cycle);
        }
        Ok(out)
    }

    /// Return the preferred parent for tree rendering: the
    /// content-coupled (`depends_on` / reference) parent sorted
    /// alphabetically, falling back to the alphabetically-first
    /// ordering-only parent (e.g. a synthetic phase edge).
    /// Returns `None` for root nodes.
    pub fn primary_parent(&self, name: &str) -> Option<String> {
        let idx = *self.indices.get(name)?;
        let mut dep_parents: Vec<String> = Vec::new();
        let mut ord_parents: Vec<String> = Vec::new();
        for edge in self.graph.edges_directed(idx, Direction::Incoming) {
            let parent = self.graph[edge.source()].name.clone();
            match edge.weight() {
                EdgeKind::Dependency => dep_parents.push(parent),
                EdgeKind::Ordering => ord_parents.push(parent),
            }
        }
        dep_parents.sort();
        ord_parents.sort();
        dep_parents
            .into_iter()
            .next()
            .or_else(|| ord_parents.into_iter().next())
    }

    /// Return block names for a target in topological order.
    /// Includes all transitive dependencies.
    pub fn target_order(&self, target: &str) -> Result<Vec<String>, DagError> {
        // Try as a named target first, then fall back to a block name.
        let block_names: Vec<String> = if let Some(t) = self.targets.get(target) {
            t.blocks.clone()
        } else if self.indices.contains_key(target) {
            vec![target.to_owned()]
        } else {
            return Err(DagError::UnknownBlock(target.into()));
        };

        let mut needed = HashSet::new();
        for name in &block_names {
            let block_name = name.split('.').next().unwrap_or(name);
            if let Some(&idx) = self.indices.get(block_name) {
                collect_transitive_deps(&self.graph, idx, &mut needed);
            }
        }

        let all = self.topo_order()?;
        Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
    }

    /// Get a node by name.
    pub fn get_node(&self, name: &str) -> Option<&DagNode> {
        self.indices.get(name).map(|&idx| &self.graph[idx])
    }

    /// Get a mutable node by name.
    pub fn get_node_mut(&mut self, name: &str) -> Option<&mut DagNode> {
        self.indices.get(name).copied().map(|idx| &mut self.graph[idx])
    }

    /// Get all targets with their docs.
    pub fn targets(&self) -> &HashMap<String, DagTarget> {
        &self.targets
    }

    /// Return test blocks and their transitive dependencies in topological order.
    pub fn test_order(&self) -> Result<Vec<String>, DagError> {
        use crate::provider::ResourceKind;
        let mut needed = HashSet::new();
        for &idx in self.indices.values() {
            if self.graph[idx].resource.kind() == ResourceKind::Test {
                collect_transitive_deps(&self.graph, idx, &mut needed);
            }
        }
        let all = self.topo_order()?;
        Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
    }

    /// Get all block names.
    pub fn block_names(&self) -> Vec<String> {
        self.indices.keys().cloned().collect()
    }

    /// Check whether a block exists.
    pub fn has_block(&self, name: &str) -> bool {
        self.indices.contains_key(name)
    }

    /// Get all parent block names (both dependency and ordering edges).
    pub fn deps(&self, name: &str) -> Vec<String> {
        let Some(&idx) = self.indices.get(name) else {
            return vec![];
        };
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .map(|n| self.graph[n].name.clone())
            .collect()
    }

    /// Get parent block names that are content-coupled (dependency edges only).
    /// These are included in the content hash computation.
    pub fn content_deps(&self, name: &str) -> Vec<String> {
        let Some(&idx) = self.indices.get(name) else {
            return vec![];
        };
        let mut result = Vec::new();
        for edge in self.graph.edges_directed(idx, petgraph::Direction::Incoming) {
            if *edge.weight() == EdgeKind::Dependency {
                result.push(self.graph[edge.source()].name.clone());
            }
        }
        result
    }

    /// Get all child block names (blocks that depend on this one).
    pub fn dependents(&self, name: &str) -> Vec<String> {
        let Some(&idx) = self.indices.get(name) else {
            return vec![];
        };
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing)
            .map(|n| self.graph[n].name.clone())
            .collect()
    }

    /// Get the depth of a node (longest path from a root).
    pub fn depth(&self, name: &str) -> usize {
        let Some(&idx) = self.indices.get(name) else {
            return 0;
        };
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .map(|n| self.depth(&self.graph[n].name) + 1)
            .max()
            .unwrap_or(0)
    }
}

impl Default for Dag {
    fn default() -> Self {
        Self::new()
    }
}

fn collect_transitive_deps(graph: &DiGraph<DagNode, EdgeKind>, node: NodeIndex, result: &mut HashSet<String>) {
    let name = &graph[node].name;
    if !result.insert(name.clone()) {
        return;
    }
    for neighbor in graph.neighbors_directed(node, petgraph::Direction::Incoming) {
        collect_transitive_deps(graph, neighbor, result);
    }
}

/// Extract block names referenced in field expressions via dotted refs.
/// Returns only the root name (e.g., "server" from "server.path").
pub fn collect_block_refs(fields: &[Field]) -> HashSet<String> {
    let mut refs = HashSet::new();
    for field in fields {
        collect_expr_refs(&field.value, &mut refs);
    }
    refs
}

/// Extract explicit `depends_on` entries from fields.
pub fn collect_depends_on(fields: &[Field]) -> Vec<String> {
    for field in fields {
        if field.name == "depends_on"
            && let Expr::List(items) = &field.value
        {
            return items
                .iter()
                .filter_map(|e| match e {
                    Expr::Ref(parts) => Some(parts[0].clone()),
                    _ => None,
                })
                .collect();
        }
    }
    vec![]
}

/// Extract explicit `after` entries from fields (ordering-only edges).
pub fn collect_after(fields: &[Field]) -> Vec<String> {
    for field in fields {
        if field.name == "after"
            && let Expr::List(items) = &field.value
        {
            return items
                .iter()
                .filter_map(|e| match e {
                    Expr::Ref(parts) => Some(parts[0].clone()),
                    _ => None,
                })
                .collect();
        }
    }
    vec![]
}

/// Collect ALL variable/block root names referenced in field expressions,
/// including single-part refs. Used for static validation.
pub fn collect_all_refs(fields: &[Field]) -> HashSet<String> {
    let mut refs = HashSet::new();
    for field in fields {
        if field.name == "depends_on" || field.name == "after" {
            continue; // validated separately
        }
        collect_all_expr_refs(&field.value, &mut refs);
    }
    refs
}

fn collect_all_expr_refs(expr: &Expr, refs: &mut HashSet<String>) {
    match expr {
        Expr::Ref(parts) => {
            refs.insert(parts[0].clone());
        }
        Expr::Str(parts) => {
            for part in parts {
                if let StringPart::Interpolation(e) = part {
                    collect_all_expr_refs(e, refs);
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_all_expr_refs(item, refs);
            }
        }
        Expr::Map(fields) => {
            for field in fields {
                collect_all_expr_refs(&field.value, refs);
            }
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_all_expr_refs(arg, refs);
            }
        }
        Expr::Pipe(inner, _, args) => {
            collect_all_expr_refs(inner, refs);
            for arg in args {
                collect_all_expr_refs(arg, refs);
            }
        }
        Expr::If(cond, then_val, else_val) => {
            collect_all_expr_refs(cond, refs);
            collect_all_expr_refs(then_val, refs);
            collect_all_expr_refs(else_val, refs);
        }
        Expr::BinOp(lhs, _, rhs) | Expr::Add(lhs, rhs) => {
            collect_all_expr_refs(lhs, refs);
            collect_all_expr_refs(rhs, refs);
        }
        _ => {}
    }
}

fn collect_expr_refs(expr: &Expr, refs: &mut HashSet<String>) {
    match expr {
        Expr::Ref(parts) if parts.len() > 1 => {
            refs.insert(parts[0].clone());
        }
        Expr::Str(parts) => {
            for part in parts {
                if let StringPart::Interpolation(e) = part {
                    collect_expr_refs(e, refs);
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_expr_refs(item, refs);
            }
        }
        Expr::Map(fields) => {
            for field in fields {
                collect_expr_refs(&field.value, refs);
            }
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_expr_refs(arg, refs);
            }
        }
        Expr::Pipe(inner, _, args) => {
            collect_expr_refs(inner, refs);
            for arg in args {
                collect_expr_refs(arg, refs);
            }
        }
        Expr::If(cond, then_val, else_val) => {
            collect_expr_refs(cond, refs);
            collect_expr_refs(then_val, refs);
            collect_expr_refs(else_val, refs);
        }
        Expr::BinOp(lhs, _, rhs) | Expr::Add(lhs, rhs) => {
            collect_expr_refs(lhs, refs);
            collect_expr_refs(rhs, refs);
        }
        _ => {}
    }
}
