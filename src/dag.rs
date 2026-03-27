use std::collections::{HashMap, HashSet};

use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::ast::{Expr, Field, StringPart};
use crate::provider::DynResource;

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("duplicate block name: {0}")]
    DuplicateBlock(String),
    #[error("unknown block: {0}")]
    UnknownBlock(String),
    #[error("dependency cycle detected")]
    Cycle,
}

/// A node in the dependency graph.
pub struct DagNode {
    pub name: String,
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
    graph: DiGraph<DagNode, ()>,
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

    /// Add a dependency edge: `from` must complete before `to`.
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<(), DagError> {
        let from_idx = self
            .indices
            .get(from)
            .ok_or_else(|| DagError::UnknownBlock(from.into()))?;
        let to_idx = self.indices.get(to).ok_or_else(|| DagError::UnknownBlock(to.into()))?;
        self.graph.add_edge(*from_idx, *to_idx, ());
        Ok(())
    }

    /// Register a target.
    pub fn add_target(&mut self, name: String, blocks: Vec<String>, doc: Option<String>) {
        self.targets.insert(name, DagTarget { blocks, doc });
    }

    /// Validate the graph has no cycles.
    pub fn validate(&self) -> Result<(), DagError> {
        toposort(&self.graph, None).map_err(|_| DagError::Cycle)?;
        Ok(())
    }

    /// Return block names in topological order (dependencies first).
    pub fn topo_order(&self) -> Result<Vec<String>, DagError> {
        let sorted = toposort(&self.graph, None).map_err(|_| DagError::Cycle)?;
        Ok(sorted.into_iter().map(|idx| self.graph[idx].name.clone()).collect())
    }

    /// Return block names for a target in topological order.
    /// Includes all transitive dependencies.
    pub fn target_order(&self, target: &str) -> Result<Vec<String>, DagError> {
        let t = self
            .targets
            .get(target)
            .ok_or_else(|| DagError::UnknownBlock(format!("target '{target}'")))?;
        let block_names = &t.blocks;

        let mut needed = HashSet::new();
        for name in block_names {
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

    /// Get all block names.
    pub fn block_names(&self) -> Vec<String> {
        self.indices.keys().cloned().collect()
    }

    /// Check whether a block exists.
    pub fn has_block(&self, name: &str) -> bool {
        self.indices.contains_key(name)
    }

    /// Get the names of blocks that `name` depends on (direct dependencies).
    pub fn deps(&self, name: &str) -> Vec<String> {
        let Some(&idx) = self.indices.get(name) else {
            return vec![];
        };
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
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

fn collect_transitive_deps(graph: &DiGraph<DagNode, ()>, node: NodeIndex, result: &mut HashSet<String>) {
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
