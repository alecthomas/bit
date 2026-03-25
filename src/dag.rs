use std::collections::{HashMap, HashSet};

use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::ast::{Block, Expr, Module, Statement, StringPart, Target};

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("duplicate block name: {0}")]
    DuplicateBlock(String),
    #[error("unknown block referenced: {0}")]
    UnknownBlock(String),
    #[error("dependency cycle detected")]
    Cycle,
}

/// A node in the dependency graph.
#[derive(Debug, Clone)]
pub struct DagNode {
    pub name: String,
    pub block: Block,
}

/// The dependency graph of blocks.
pub struct Dag {
    graph: DiGraph<DagNode, ()>,
    indices: HashMap<String, NodeIndex>,
    targets: HashMap<String, Vec<String>>,
}

impl Dag {
    /// Build a DAG from a parsed module.
    pub fn build(module: &Module) -> Result<Self, DagError> {
        let mut graph = DiGraph::new();
        let mut indices = HashMap::new();
        let mut targets = HashMap::new();

        // Collect all blocks and targets
        let blocks: Vec<&Block> = module
            .statements
            .iter()
            .filter_map(|s| match s {
                Statement::Block(b) => Some(b),
                _ => None,
            })
            .collect();

        // Add nodes
        for block in &blocks {
            if indices.contains_key(&block.name) {
                return Err(DagError::DuplicateBlock(block.name.clone()));
            }
            let idx = graph.add_node(DagNode {
                name: block.name.clone(),
                block: (*block).clone(),
            });
            indices.insert(block.name.clone(), idx);
        }

        // Add edges from references
        for block in &blocks {
            let deps = collect_block_deps(block);
            let block_idx = indices[&block.name];
            for dep in deps {
                // Only the first part of a dotted ref is the block name
                let dep_block = dep.as_str();
                if let Some(&dep_idx) = indices.get(dep_block) {
                    // dep must run before block
                    graph.add_edge(dep_idx, block_idx, ());
                }
                // Refs to unknown names are fine — could be let bindings or params
            }
        }

        // Collect targets
        for stmt in &module.statements {
            if let Statement::Target(Target { name, blocks: refs }) = stmt {
                targets.insert(name.clone(), refs.clone());
            }
        }

        // Check for cycles
        if toposort(&graph, None).is_err() {
            return Err(DagError::Cycle);
        }

        Ok(Dag {
            graph,
            indices,
            targets,
        })
    }

    /// Return block names in topological order (dependencies first).
    pub fn topo_order(&self) -> Result<Vec<String>, DagError> {
        let sorted = toposort(&self.graph, None).map_err(|_| DagError::Cycle)?;
        Ok(sorted.into_iter().map(|idx| self.graph[idx].name.clone()).collect())
    }

    /// Return block names for a target in topological order.
    /// Includes all transitive dependencies.
    pub fn target_order(&self, target: &str) -> Result<Vec<String>, DagError> {
        let block_names = self
            .targets
            .get(target)
            .ok_or_else(|| DagError::UnknownBlock(format!("target '{target}'")))?;

        // Collect all transitive deps for the target's blocks
        let mut needed = HashSet::new();
        for name in block_names {
            // Target refs can be dotted (e.g., "staging.deploy"), take first part
            let block_name = name.split('.').next().unwrap_or(name);
            if let Some(&idx) = self.indices.get(block_name) {
                collect_transitive_deps(&self.graph, idx, &mut needed);
            }
        }

        // Return in topo order, filtered to needed
        let all = self.topo_order()?;
        Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
    }

    /// Get a block by name.
    pub fn get_block(&self, name: &str) -> Option<&Block> {
        self.indices.get(name).map(|&idx| &self.graph[idx].block)
    }

    /// Get all target names.
    pub fn targets(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }

    /// Get all block names.
    pub fn block_names(&self) -> Vec<String> {
        self.indices.keys().cloned().collect()
    }
}

/// Collect transitive dependencies of a node (including itself).
fn collect_transitive_deps(graph: &DiGraph<DagNode, ()>, node: NodeIndex, result: &mut HashSet<String>) {
    let name = &graph[node].name;
    if !result.insert(name.clone()) {
        return;
    }
    for neighbor in graph.neighbors_directed(node, petgraph::Direction::Incoming) {
        collect_transitive_deps(graph, neighbor, result);
    }
}

/// Extract block names referenced in a block's field expressions.
/// Returns only the root name of dotted refs (e.g., "server" from "server.path").
fn collect_block_deps(block: &Block) -> HashSet<String> {
    let mut deps = HashSet::new();
    for field in &block.fields {
        collect_expr_refs(&field.value, &mut deps);
    }
    // Remove self-references
    deps.remove(&block.name);
    deps
}

fn collect_expr_refs(expr: &Expr, refs: &mut HashSet<String>) {
    match expr {
        Expr::Ref(parts) => {
            if parts.len() > 1 {
                // Dotted ref like block.field — the block is a dependency
                refs.insert(parts[0].clone());
            }
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
        Expr::BinOp(lhs, _, rhs) => {
            collect_expr_refs(lhs, refs);
            collect_expr_refs(rhs, refs);
        }
        Expr::Add(lhs, rhs) => {
            collect_expr_refs(lhs, refs);
            collect_expr_refs(rhs, refs);
        }
        Expr::Int(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

/// Check if a field expression contains a `depends_on` list and extract block names.
pub fn extract_depends_on(block: &Block) -> Vec<String> {
    for field in &block.fields {
        if field.name == "depends_on"
            && let Expr::List(items) = &field.value
        {
            return items
                .iter()
                .filter_map(|e| match e {
                    Expr::Ref(parts) => Some(parts.join(".")),
                    _ => None,
                })
                .collect();
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn parse_and_build(input: &str) -> Result<Dag, DagError> {
        let module = parser::parse(input).expect("parse failed");
        Dag::build(&module)
    }

    #[test]
    fn simple_dependency() {
        let input = concat!(
            "server = go.binary { main = \"./cmd/server\" }\n",
            "image = docker.image { tag = server.path }\n",
        );
        let dag = parse_and_build(input).unwrap();
        let order = dag.topo_order().unwrap();
        let si = order.iter().position(|n| n == "server").unwrap();
        let ii = order.iter().position(|n| n == "image").unwrap();
        assert!(si < ii, "server must come before image");
    }

    #[test]
    fn no_dependencies() {
        let input = concat!(
            "a = exec {\n  command = \"echo a\"\n  output = \"a\"\n}\n",
            "b = exec {\n  command = \"echo b\"\n  output = \"b\"\n}\n",
        );
        let dag = parse_and_build(input).unwrap();
        let order = dag.topo_order().unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn diamond_dependency() {
        let input = concat!(
            "a = exec {\n  command = \"a\"\n  output = \"a\"\n}\n",
            "b = exec {\n  command = a.path\n  output = \"b\"\n}\n",
            "c = exec {\n  command = a.path\n  output = \"c\"\n}\n",
            "d = exec {\n  command = \"${b.path} ${c.path}\"\n  output = \"d\"\n}\n",
        );
        let dag = parse_and_build(input).unwrap();
        let order = dag.topo_order().unwrap();
        let ai = order.iter().position(|n| n == "a").unwrap();
        let bi = order.iter().position(|n| n == "b").unwrap();
        let ci = order.iter().position(|n| n == "c").unwrap();
        let di = order.iter().position(|n| n == "d").unwrap();
        assert!(ai < bi);
        assert!(ai < ci);
        assert!(bi < di);
        assert!(ci < di);
    }

    #[test]
    fn cycle_detected() {
        let input = concat!(
            "a = exec {\n  command = b.path\n  output = \"a\"\n}\n",
            "b = exec {\n  command = a.path\n  output = \"b\"\n}\n",
        );
        let result = parse_and_build(input);
        assert!(matches!(result, Err(DagError::Cycle)));
    }

    #[test]
    fn duplicate_block() {
        let input = concat!(
            "a = exec {\n  command = \"x\"\n  output = \"a\"\n}\n",
            "a = exec {\n  command = \"y\"\n  output = \"b\"\n}\n",
        );
        let result = parse_and_build(input);
        assert!(matches!(result, Err(DagError::DuplicateBlock(_))));
    }

    #[test]
    fn target_order() {
        let input = concat!(
            "a = exec {\n  command = \"a\"\n  output = \"a\"\n}\n",
            "b = exec {\n  command = a.path\n  output = \"b\"\n}\n",
            "c = exec {\n  command = \"c\"\n  output = \"c\"\n}\n",
            "target build = [b]\n",
        );
        let dag = parse_and_build(input).unwrap();
        let order = dag.target_order("build").unwrap();
        assert!(order.contains(&"a".to_string()), "should include dep 'a'");
        assert!(order.contains(&"b".to_string()), "should include target 'b'");
        assert!(!order.contains(&"c".to_string()), "should not include unrelated 'c'");
    }

    #[test]
    fn get_block() {
        let input = "server = go.binary { main = \"./cmd/server\" }\n";
        let dag = parse_and_build(input).unwrap();
        let block = dag.get_block("server").unwrap();
        assert_eq!(block.provider, "go");
        assert_eq!(block.resource, "binary");
    }

    #[test]
    fn targets_list() {
        let input = concat!(
            "a = exec {\n  command = \"a\"\n  output = \"a\"\n}\n",
            "target build = [a]\n",
            "target test = [a]\n",
        );
        let dag = parse_and_build(input).unwrap();
        let mut targets = dag.targets();
        targets.sort();
        assert_eq!(targets, vec!["build", "test"]);
    }

    #[test]
    fn interpolation_deps() {
        let input = concat!(
            "server = exec {\n  command = \"build\"\n  output = \"server\"\n}\n",
            "image = exec {\n  command = \"docker build -t ${server.path}\"\n  output = \"image\"\n}\n",
        );
        let dag = parse_and_build(input).unwrap();
        let order = dag.topo_order().unwrap();
        let si = order.iter().position(|n| n == "server").unwrap();
        let ii = order.iter().position(|n| n == "image").unwrap();
        assert!(si < ii);
    }

    #[test]
    fn extract_depends_on_field() {
        let input = "deploy = exec {\n  command = \"deploy\"\n  output = \"d\"\n  depends_on = [migrations]\n}\n";
        let module = parser::parse(input).unwrap();
        if let Statement::Block(block) = &module.statements[0] {
            let deps = extract_depends_on(block);
            assert_eq!(deps, vec!["migrations"]);
        } else {
            panic!("expected block");
        }
    }
}
