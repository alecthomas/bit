use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use petgraph::Direction;
use petgraph::algo::{tarjan_scc, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::ast::{Block, Expr, Field, Phase, StringPart, Target};
use crate::expr::{self, EvalError, Scope};
use crate::provider::DynResource;
use crate::value::{Type, Value};

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
    #[error("dependency cycle detected: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
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
    /// When true, this block is excluded from the `...` selector and must be
    /// named explicitly to run. Independent of `protected`.
    pub explicit: bool,
    /// Expanded nodes from the same source block share this identity so the
    /// scheduler can enforce that block's concurrency limit across all slices.
    pub concurrency_group: String,
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
    block_declarations: HashMap<String, Block>,
    target_declarations: HashMap<String, Target>,
}

impl Dag {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            indices: HashMap::new(),
            targets: HashMap::new(),
            block_declarations: HashMap::new(),
            target_declarations: HashMap::new(),
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
        if !self
            .graph
            .edges_connecting(*from_idx, *to_idx)
            .any(|edge| *edge.weight() == kind)
        {
            self.graph.add_edge(*from_idx, *to_idx, kind);
        }
        Ok(())
    }

    /// Register a target.
    pub fn add_target(&mut self, name: String, blocks: Vec<String>, doc: Option<String>) {
        self.targets.insert(name, DagTarget { blocks, doc });
    }

    /// Register a parameterized block declaration discovered in an imported module.
    pub(crate) fn add_block_declaration(&mut self, block: Block) {
        self.block_declarations.insert(block.name.clone(), block);
    }

    pub(crate) fn block_declarations(&self) -> &HashMap<String, Block> {
        &self.block_declarations
    }

    /// Register a target declaration discovered in an imported module.
    pub(crate) fn add_target_declaration(&mut self, target: Target) {
        self.target_declarations.insert(target.name.clone(), target);
    }

    pub fn target_declarations(&self) -> &HashMap<String, Target> {
        &self.target_declarations
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
        toposort(&self.graph, None).map_err(|_| self.cycle_error())?;
        for (name, target) in &self.targets {
            for block in &target.blocks {
                if !self.indices.contains_key(block) {
                    return Err(DagError::UnknownTargetBlock(name.clone(), block.clone()));
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
            return Err(self.cycle_error());
        }
        Ok(out)
    }

    fn cycle_error(&self) -> DagError {
        let mut components: Vec<_> = tarjan_scc(&self.graph)
            .into_iter()
            .filter(|component| {
                component.len() > 1
                    || component
                        .first()
                        .is_some_and(|node| self.graph.find_edge(*node, *node).is_some())
            })
            .collect();
        components.sort_by_key(|component| {
            component
                .iter()
                .map(|node| self.graph[*node].name.as_str())
                .min()
                .unwrap_or_default()
                .to_owned()
        });

        let Some(component) = components.first() else {
            return DagError::Cycle(Vec::new());
        };
        let members: HashSet<_> = component.iter().copied().collect();
        let Some(&start) = component.iter().min_by_key(|node| self.graph[**node].name.as_str()) else {
            return DagError::Cycle(Vec::new());
        };
        let mut visited = HashSet::from([start]);
        let mut path = vec![start];
        if !self.find_cycle_path(start, start, &members, &mut visited, &mut path) {
            path.push(start);
        }
        DagError::Cycle(path.into_iter().map(|node| self.graph[node].name.clone()).collect())
    }

    fn find_cycle_path(
        &self,
        current: NodeIndex,
        start: NodeIndex,
        members: &HashSet<NodeIndex>,
        visited: &mut HashSet<NodeIndex>,
        path: &mut Vec<NodeIndex>,
    ) -> bool {
        let mut neighbors: Vec<_> = self
            .graph
            .neighbors_directed(current, Direction::Outgoing)
            .filter(|node| members.contains(node))
            .collect();
        neighbors.sort_by(|left, right| self.graph[*left].name.cmp(&self.graph[*right].name));

        for neighbor in neighbors {
            if neighbor == start {
                path.push(start);
                return true;
            }
            if visited.insert(neighbor) {
                path.push(neighbor);
                if self.find_cycle_path(neighbor, start, members, visited, path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }

    /// Topological order with `explicit` blocks filtered out.
    ///
    /// This is the canonical expansion of the `...` selector (and of the
    /// no-target/no-default fallback): "every block that is selectable
    /// without being named." `explicit` blocks remain reachable by name or
    /// as transitive dependencies of named targets, but never participate in
    /// a bulk "run everything" sweep.
    pub fn select_all(&self) -> Result<Vec<String>, DagError> {
        Ok(self
            .topo_order()?
            .into_iter()
            .filter(|n| !self.indices.get(n).is_some_and(|idx| self.graph[*idx].explicit))
            .collect())
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
            if let Some(&idx) = self.indices.get(name) {
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

    /// Return block names for a target (or single block) plus all of their
    /// transitive dependents (the blocks that depend on them), in topological
    /// order (dependencies first — callers typically reverse this for teardown).
    ///
    /// # Arguments
    ///
    /// * `target` - A target name or a block name.
    ///
    /// # Errors
    ///
    /// Returns [`DagError::UnknownBlock`] if `target` is neither a known target
    /// nor a known block name, and [`DagError::Cycle`] if the graph contains a
    /// cycle.
    pub fn transitive_dependents(&self, target: &str) -> Result<Vec<String>, DagError> {
        // Resolve either a named target (set of blocks) or a single block.
        let block_names: Vec<String> = if let Some(t) = self.targets.get(target) {
            t.blocks.clone()
        } else if self.indices.contains_key(target) {
            vec![target.to_owned()]
        } else {
            return Err(DagError::UnknownBlock(target.into()));
        };

        let mut needed = HashSet::new();
        for name in &block_names {
            if let Some(&idx) = self.indices.get(name) {
                collect_transitive_dependents(&self.graph, idx, &mut needed);
            }
        }

        let all = self.topo_order()?;
        Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
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

/// Walk outgoing edges from `node`, inserting every reachable block (inclusive)
/// into `result`. These are the blocks that depend, directly or transitively,
/// on `node` — i.e. blocks that must be destroyed before `node` can be.
fn collect_transitive_dependents(graph: &DiGraph<DagNode, EdgeKind>, node: NodeIndex, result: &mut HashSet<String>) {
    let name = &graph[node].name;
    if !result.insert(name.clone()) {
        return;
    }
    for neighbor in graph.neighbors_directed(node, petgraph::Direction::Outgoing) {
        collect_transitive_dependents(graph, neighbor, result);
    }
}

/// Extract block names referenced in field expressions via dotted refs.
/// Returns only the root name (e.g., "server" from "server.path").
pub fn collect_block_refs(fields: &[Field], scope: &Scope) -> Result<HashSet<String>, EvalError> {
    let mut refs = HashSet::new();
    for field in fields {
        if field.name == "depends_on" || field.name == "after" || field.name == "concurrency" {
            continue;
        }
        collect_expr_refs(&field.value, scope, &mut refs)?;
    }
    Ok(refs)
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
                    Expr::BlockRef(name) => Some(name.clone()),
                    Expr::MatrixRef { .. } => Some(e.to_string()),
                    _ => None,
                })
                .collect();
        }
    }
    vec![]
}

/// Resolve a dependency field that is either a literal list of block
/// references or a configuration-time expression returning `[block]`.
pub fn collect_dependency_refs(fields: &[Field], field_name: &str, scope: &Scope) -> Result<Vec<String>, EvalError> {
    let Some(field) = fields.iter().find(|field| field.name == field_name) else {
        return Ok(Vec::new());
    };

    if let Expr::List(items) = &field.value {
        if items.iter().any(|item| matches!(item, Expr::BlockCall { .. })) {
            // Parameterized calls are resolved by the materialization pass.
            return Ok(Vec::new());
        }
        let mut references = Vec::with_capacity(items.len());
        for item in items {
            let reference = match item {
                Expr::Ref(parts) => Some(parts[0].clone()),
                Expr::BlockRef(name) => Some(name.clone()),
                Expr::MatrixRef { name, keys, fields } if fields.is_empty() => {
                    Some(expr::eval_matrix_ref_name(name, keys, scope)?)
                }
                _ => None,
            };
            let Some(reference) = reference else {
                references.clear();
                break;
            };
            references.push(reference);
        }
        if references.len() == items.len() {
            return Ok(references);
        }
    }

    let value = match expr::eval(&field.value, scope) {
        Ok(value) => value,
        Err(EvalError::UnmaterializedBlockCall(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    match value {
        Value::List(Type::BlockRef, references) => references
            .into_iter()
            .map(|reference| match reference {
                Value::BlockRef(name) => Ok(name),
                value => Err(EvalError::Type(format!(
                    "{field_name} must return block references, got {value}"
                ))),
            })
            .collect(),
        value => Err(EvalError::Type(format!(
            "{field_name} must be a list of block references, got {value}"
        ))),
    }
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
                    Expr::BlockRef(name) => Some(name.clone()),
                    Expr::MatrixRef { .. } => Some(e.to_string()),
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
        if field.name == "depends_on" || field.name == "after" || field.name == "concurrency" {
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
        Expr::BlockRef(name) => {
            refs.insert(name.clone());
        }
        Expr::MatrixRef { name, keys, .. } => {
            refs.insert(name.clone());
            for key in keys {
                collect_all_expr_refs(key, refs);
            }
        }
        Expr::BlockCall { name, args, .. } => {
            refs.insert(name.clone());
            for arg in args {
                collect_all_expr_refs(&arg.value, refs);
            }
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

fn collect_expr_refs(expr: &Expr, scope: &Scope, refs: &mut HashSet<String>) -> Result<(), EvalError> {
    match expr {
        Expr::Ref(parts) if parts.len() > 1 => {
            refs.insert(parts[0].clone());
        }
        Expr::BlockRef(name) => {
            refs.insert(name.clone());
        }
        Expr::MatrixRef { name, keys, fields: _ } => {
            refs.insert(expr::eval_matrix_ref_name(name, keys, scope)?);
            for key in keys {
                collect_expr_refs(key, scope, refs)?;
            }
        }
        Expr::BlockCall { name, args, .. } => {
            refs.insert(name.clone());
            for arg in args {
                collect_expr_refs(&arg.value, scope, refs)?;
            }
        }
        Expr::Str(parts) => {
            for part in parts {
                if let StringPart::Interpolation(e) = part {
                    collect_expr_refs(e, scope, refs)?;
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_expr_refs(item, scope, refs)?;
            }
        }
        Expr::Map(fields) => {
            for field in fields {
                collect_expr_refs(&field.value, scope, refs)?;
            }
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_expr_refs(arg, scope, refs)?;
            }
        }
        Expr::Pipe(inner, _, args) => {
            collect_expr_refs(inner, scope, refs)?;
            for arg in args {
                collect_expr_refs(arg, scope, refs)?;
            }
        }
        Expr::If(cond, then_val, else_val) => {
            collect_expr_refs(cond, scope, refs)?;
            collect_expr_refs(then_val, scope, refs)?;
            collect_expr_refs(else_val, scope, refs)?;
        }
        Expr::BinOp(lhs, _, rhs) | Expr::Add(lhs, rhs) => {
            collect_expr_refs(lhs, scope, refs)?;
            collect_expr_refs(rhs, scope, refs)?;
        }
        _ => {}
    }
    Ok(())
}
