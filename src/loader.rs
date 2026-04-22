use std::path::Path;

use std::collections::HashMap;

use crate::ast::{Module, Statement};
use crate::dag::{Dag, DagError, DagNode, collect_after, collect_all_refs, collect_block_refs, collect_depends_on};
use crate::expr::{self, EvalError, Scope};
use crate::matrix;
use crate::module;
use crate::provider::ProviderRegistry;
use crate::state::{StateError, StateStore};
use crate::value::{Map, Value, validate_type};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{pos}: {source}")]
    Eval { pos: crate::ast::Pos, source: EvalError },
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("{0}")]
    State(#[from] StateError),
    #[error("{pos}: missing required param: {name}")]
    MissingParam { pos: crate::ast::Pos, name: String },
    #[error("{pos}: unknown provider/resource: {provider}.{resource}")]
    UnknownResource {
        pos: crate::ast::Pos,
        provider: String,
        resource: String,
    },
    #[error("{pos}: {name}: {message}")]
    TypeError {
        pos: crate::ast::Pos,
        name: String,
        message: String,
    },
    #[error("{pos}: undefined '{name}' referenced in block '{from}'")]
    UnknownBlock {
        pos: crate::ast::Pos,
        name: String,
        from: String,
    },
    #[error("unknown param: {name}")]
    UnknownParam { name: String },
    #[error("failed to load module {0}: {1}")]
    ModuleLoad(String, String),
    #[error("failed to parse module {0}: {1}")]
    ModuleParse(String, String),
    #[error("{pos}: matrix key '{name}' not found in scope")]
    MatrixKeyNotFound { pos: crate::ast::Pos, name: String },
    #[error("{pos}: matrix key '{name}' must be a list")]
    MatrixKeyNotList { pos: crate::ast::Pos, name: String },
    #[error("{pos}: name '{name}' is already used by a {existing}")]
    DuplicateName {
        pos: crate::ast::Pos,
        name: String,
        existing: &'static str,
    },
}

/// The base scope of evaluated params and let bindings, shared with the engine
/// for deferred field evaluation.
pub struct BaseScope {
    pub scope: Scope,
    /// Params that were declared without defaults and not provided via -p.
    /// Active blocks referencing these will produce errors at execution time.
    pub missing_params: std::collections::HashSet<String>,
}

/// Parse a module and build a fully wired DAG.
///
/// - Evaluates params and let bindings into a base scope
/// - Looks up resource implementations from the registry
/// - Loads prior state from the store
/// - Builds dependency edges from field refs and depends_on
/// - Validates no cycles
///
/// Field expressions are NOT evaluated here — they're deferred to execution
/// time because they may reference upstream block outputs.
pub fn load(
    module: &Module,
    params: &Map,
    registry: &ProviderRegistry,
    store: &dyn StateStore,
    root: &Path,
) -> Result<(Dag, BaseScope), LoadError> {
    let mut scope = Scope::new();
    let mut dag = Dag::new();
    let mut block_names = Vec::new();
    let mut matrix_blocks: HashMap<String, Vec<String>> = HashMap::new();
    let mut deferred_matrix: Vec<crate::ast::Block> = Vec::new();
    let mut missing_params: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut declared_params: std::collections::HashSet<String> = std::collections::HashSet::new();

    for stmt in &module.statements {
        match stmt {
            Statement::Param(p) => {
                declared_params.insert(p.name.clone());
                let value = if let Some(v) = params.get(&p.name) {
                    Some(v.clone())
                } else if let Some(default) = &p.default {
                    match expr::eval(default, &scope) {
                        Ok(v) => Some(v),
                        Err(_) => {
                            // Default depends on a missing param — defer
                            missing_params.insert(p.name.clone());
                            None
                        }
                    }
                } else {
                    // Required param not provided — defer, don't error
                    missing_params.insert(p.name.clone());
                    None
                };
                if let Some(value) = value {
                    validate_type(&value, &p.typ).map_err(|message| LoadError::TypeError {
                        pos: p.pos.clone(),
                        name: p.name.clone(),
                        message,
                    })?;
                    scope
                        .define(&p.name, expr::SymbolKind::Param, value)
                        .map_err(|existing| LoadError::DuplicateName {
                            pos: p.pos.clone(),
                            name: p.name.clone(),
                            existing: existing.as_str(),
                        })?;
                }
            }
            Statement::Let(l) => {
                match expr::eval(&l.value, &scope) {
                    Ok(value) => {
                        if let Some(typ) = &l.typ {
                            validate_type(&value, typ).map_err(|message| LoadError::TypeError {
                                pos: l.pos.clone(),
                                name: l.name.clone(),
                                message,
                            })?;
                        }
                        scope
                            .define(&l.name, expr::SymbolKind::Let, value)
                            .map_err(|existing| LoadError::DuplicateName {
                                pos: l.pos.clone(),
                                name: l.name.clone(),
                                existing: existing.as_str(),
                            })?;
                    }
                    Err(_) => {
                        // Let depends on a missing param — defer
                        missing_params.insert(l.name.clone());
                    }
                }
            }
            Statement::Block(b) => {
                if !b.matrix_keys.is_empty() {
                    // Defer matrix blocks — full define happens in expand_matrix.
                    // Check here only for conflicts with params/lets.
                    if let Some(existing) = scope.kind(&b.name) {
                        return Err(LoadError::DuplicateName {
                            pos: b.pos.clone(),
                            name: b.name.clone(),
                            existing: existing.as_str(),
                        });
                    }
                    matrix_blocks.insert(b.name.clone(), b.matrix_keys.clone());
                    deferred_matrix.push(b.clone());
                    block_names.push(b.name.clone());
                } else if let Some(module_path) = module::resolve_module_path(root, &b.provider, &b.resource) {
                    // Full define happens in expand_module.
                    // Check here only for conflicts with params/lets.
                    if let Some(existing) = scope.kind(&b.name) {
                        return Err(LoadError::DuplicateName {
                            pos: b.pos.clone(),
                            name: b.name.clone(),
                            existing: existing.as_str(),
                        });
                    }
                    let mut ctx = module::ExpandContext {
                        scope: &mut scope,
                        registry,
                        store,
                        dag: &mut dag,
                        root,
                    };
                    module::expand_module(&b.name, &module_path, &b.fields, &mut ctx)?;
                    block_names.push(b.name.clone());
                } else {
                    let resource =
                        registry
                            .get_resource(&b.provider, &b.resource)
                            .ok_or_else(|| LoadError::UnknownResource {
                                pos: b.pos.clone(),
                                provider: b.provider.clone(),
                                resource: b.resource.clone(),
                            })?;

                    let prior_state = store.load(&b.name)?;

                    dag.add_node(DagNode {
                        pos: b.pos.clone(),
                        name: b.name.clone(),
                        doc: b.doc.clone(),
                        phase: b.phase,
                        provider: b.provider.clone(),
                        resource_name: b.resource.clone(),
                        protected: b.protected,
                        fields: b.fields.clone(),
                        resource,
                        prior_state,
                    })?;

                    block_names.push(b.name.clone());
                    scope
                        .define(&b.name, expr::SymbolKind::Block, Value::strct(Map::new()))
                        .map_err(|existing| LoadError::DuplicateName {
                            pos: b.pos.clone(),
                            name: b.name.clone(),
                            existing: existing.as_str(),
                        })?;
                }
            }
            Statement::Target(t) => {
                dag.add_target(t.name.clone(), t.blocks.clone(), t.doc.clone());
            }
            Statement::Output(_) => {
                // Outputs are deferred — they reference block outputs
                // which aren't available until execution.
            }
        }
    }

    // Expand matrix blocks now that all params/lets are in scope
    for block in &deferred_matrix {
        matrix::expand_matrix(block, &mut scope, registry, store, &mut dag, &matrix_blocks)?;
    }

    // Build dependency edges from field refs, depends_on, and after.
    // Skip matrix blocks — their edges are wired during expansion.
    // For non-matrix blocks referencing a matrix block name, create edges
    // to all expanded slices.
    for stmt in &module.statements {
        if let Statement::Block(b) = stmt {
            if !b.matrix_keys.is_empty() {
                continue;
            }
            // Implicit deps from expression refs — these may reference
            // scope variables (not blocks), so we only create edges for
            // names that exist in the DAG or as matrix blocks.
            let refs = collect_block_refs(&b.fields);
            for dep in &refs {
                let resolved = resolve_dep(dep, &dag, &matrix_blocks, &scope);
                for r in &resolved {
                    if *r != b.name {
                        dag.add_dep_edge(r, &b.name)?;
                    }
                }
            }
            // Explicit depends_on — must reference known blocks.
            for dep in collect_depends_on(&b.fields) {
                let resolved = resolve_dep(&dep, &dag, &matrix_blocks, &scope);
                if resolved.is_empty() {
                    return Err(LoadError::UnknownBlock {
                        pos: b.pos.clone(),
                        name: dep,
                        from: b.name.clone(),
                    });
                }
                for r in &resolved {
                    if *r != b.name {
                        dag.add_dep_edge(r, &b.name)?;
                    }
                }
            }
            // Explicit after — must reference known blocks.
            for dep in collect_after(&b.fields) {
                let resolved = resolve_dep(&dep, &dag, &matrix_blocks, &scope);
                if resolved.is_empty() {
                    return Err(LoadError::UnknownBlock {
                        pos: b.pos.clone(),
                        name: dep,
                        from: b.name.clone(),
                    });
                }
                for r in &resolved {
                    if *r != b.name {
                        dag.add_ordering_edge(r, &b.name)?;
                    }
                }
            }
        }
    }

    // Validate that all expression refs in block fields resolve to known names.
    for stmt in &module.statements {
        if let Statement::Block(b) = stmt {
            if !b.matrix_keys.is_empty() {
                continue; // matrix blocks are rewritten, refs validated during expansion
            }
            for name in collect_all_refs(&b.fields) {
                if scope.get(&name).is_none()
                    && !dag.has_block(&name)
                    && !matrix_blocks.contains_key(&name)
                    && !missing_params.contains(&name)
                {
                    return Err(LoadError::UnknownBlock {
                        pos: b.pos.clone(),
                        name,
                        from: b.name.clone(),
                    });
                }
            }
        }
    }

    dag.wire_phase_edges();
    dag.validate()?;

    for key in params.keys() {
        if !declared_params.contains(key) {
            return Err(LoadError::UnknownParam { name: key.clone() });
        }
    }

    Ok((dag, BaseScope { scope, missing_params }))
}

/// Resolve a dependency name to actual DAG node names.
/// If the name is a matrix block, returns all expanded slice names.
/// Otherwise returns the name itself if it exists in the DAG.
fn resolve_dep(name: &str, dag: &Dag, matrix_blocks: &HashMap<String, Vec<String>>, scope: &Scope) -> Vec<String> {
    if dag.has_block(name) {
        return vec![name.to_owned()];
    }
    // If name matches a matrix block, resolve to all expanded slice names
    if matrix_blocks.contains_key(name)
        && let Some(Value::Struct(_, map)) = scope.get(name)
    {
        return map.keys().map(|k| format!("{name}[{k}]")).collect();
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::parser;
    use crate::providers::exec::ExecProvider;

    fn test_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(ExecProvider));
        reg
    }

    struct EmptyStore;
    impl StateStore for EmptyStore {
        fn load(&self, _block: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(None)
        }
        fn save(&self, _block: &str, _state: &serde_json::Value) -> Result<(), StateError> {
            Ok(())
        }
        fn remove(&self, _block: &str) -> Result<(), StateError> {
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(vec![])
        }
    }

    #[test]
    fn load_simple_block() {
        let input = r#"
server = exec {
  command = "go build"
  output = "server"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        let node = dag.get_node("server").unwrap();
        assert_eq!(node.provider, "exec");
        assert_eq!(node.resource_name, "exec");
        assert_eq!(node.fields.len(), 2);
    }

    #[test]
    fn load_with_let_binding() {
        let input = r#"
let name = "hello"
server = exec {
  command = name
  output = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        assert!(dag.get_node("server").is_some());
        assert_eq!(scope.scope.get("name").unwrap().as_str(), Some("hello"));
    }

    #[test]
    fn load_param_with_value() {
        let input = r#"
param env : string
server = exec {
  command = env
  output = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let mut params = Map::new();
        params.insert("env".into(), Value::Str("prod".into()));
        let (_dag, scope) = load(&module, &params, &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        assert_eq!(scope.scope.get("env").unwrap().as_str(), Some("prod"));
    }

    #[test]
    fn load_missing_param_deferred() {
        // Missing params don't error at load time — they're deferred
        let input = "param env : string\n";
        let module = parser::parse(input, "<test>").unwrap();
        let (_dag, base) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        assert!(base.missing_params.contains("env"));
    }

    #[test]
    fn load_unknown_param_errors() {
        let input = "param env : string\n";
        let module = parser::parse(input, "<test>").unwrap();
        let mut params = Map::new();
        params.insert("bogus".into(), Value::Str("val".into()));
        let result = load(&module, &params, &test_registry(), &EmptyStore, Path::new("."));
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("unknown param: bogus"), "got: {err}");
    }

    #[test]
    fn load_dependency_edges() {
        let input = r#"
a = exec {
  command = "build a"
  output = "a"
}
b = exec {
  command = a.path
  output = "b"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        let order = dag.topo_order().unwrap();
        let ai = order.iter().position(|n| n == "a").unwrap();
        let bi = order.iter().position(|n| n == "b").unwrap();
        assert!(ai < bi);
    }

    #[test]
    fn topo_order_is_stable_and_alphabetical() {
        // Three independent blocks defined out of alphabetical order.
        // Stable order should be alphabetical regardless of input order.
        let input = r#"
zebra = exec { command = "z" output = "z" }
apple = exec { command = "a" output = "a" }
mango = exec { command = "m" output = "m" }
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        assert_eq!(dag.topo_order().unwrap(), vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn primary_parent_prefers_dependency_over_ordering() {
        // integration-test content-depends on debug and has a phase-edge
        // ordering-dep on fmt. Primary should be debug.
        let input = r#"
pre fmt = exec { command = "fmt" output = "fmt" }
debug = exec { command = "d" output = "d" }
integration-test = exec {
  command  = "t"
  output   = "t"
  depends_on = [debug]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        assert_eq!(dag.primary_parent("integration-test"), Some("debug".into()));
        assert_eq!(dag.primary_parent("debug"), Some("fmt".into()));
        assert_eq!(dag.primary_parent("fmt"), None);
    }

    #[test]
    fn load_target() {
        let input = r#"
server = exec {
  command = "build"
  output = "out"
}
target build = [server]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        let order = dag.target_order("build").unwrap();
        assert_eq!(order, vec!["server"]);
    }

    #[test]
    fn load_cycle_detected() {
        let input = r#"
a = exec {
  command = b.path
  output = "a"
}
b = exec {
  command = a.path
  output = "b"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new("."));
        assert!(matches!(result, Err(LoadError::Dag(DagError::Cycle))));
    }

    #[test]
    fn load_unknown_provider() {
        let input = r#"
server = go.binary {
  main = "./cmd/server"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new("."));
        assert!(matches!(result, Err(LoadError::UnknownResource { .. })));
    }

    #[test]
    fn load_depends_on_creates_edge() {
        let input = r#"
migrations = exec {
  command = "migrate"
  output = "m"
}
deploy = exec {
  command = "deploy"
  output = "d"
  depends_on = [migrations]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        let order = dag.topo_order().unwrap();
        let mi = order.iter().position(|n| n == "migrations").unwrap();
        let di = order.iter().position(|n| n == "deploy").unwrap();
        assert!(mi < di);
    }

    #[test]
    fn load_target_unknown_block() {
        let input = "target build = [nonexistent]\n";
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new("."));
        assert!(matches!(result, Err(LoadError::Dag(DagError::UnknownTargetBlock(..)))));
    }

    #[test]
    fn load_target_doc() {
        let input = r#"
server = exec {
  command = "build"
  output = "out"
}
# Build the server
target build = [server]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();
        let targets = dag.targets();
        assert_eq!(targets["build"].doc.as_deref(), Some("Build the server"));
    }

    /// Create a temp dir with a module file at .bit/modules/{provider}/{resource}.bit
    fn write_module(dir: &std::path::Path, provider: &str, resource: &str, content: &str) {
        let module_dir = dir.join(".bit/modules").join(provider);
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join(format!("{resource}.bit")), content).unwrap();
    }

    #[test]
    fn load_module_creates_namespaced_blocks() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param cmd : string

inner = exec {
  command = cmd
  output = "out"
}

output result = inner.path
"#,
        );

        let input = r#"
inst = mymod {
  cmd = "echo hello"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        // Inner block is namespaced
        assert!(dag.has_block("inst.inner"));
        // Module instance block exists
        assert!(dag.has_block("inst"));
        // Module instance depends on inner block
        let order = dag.topo_order().unwrap();
        let inner_pos = order.iter().position(|n| n == "inst.inner").unwrap();
        let inst_pos = order.iter().position(|n| n == "inst").unwrap();
        assert!(inner_pos < inst_pos);
    }

    #[test]
    fn load_module_param_substitution() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param greeting : string

hello = exec {
  command = greeting
  output = "out"
}
"#,
        );

        let input = r#"
inst = mymod {
  greeting = "hi there"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        let node = dag.get_node("inst.hello").unwrap();
        // The command field should have the substituted literal value
        let cmd_field = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd_field.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("hi there".into())])
        );
    }

    #[test]
    fn load_module_inner_block_refs_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
a = exec {
  command = "build a"
  output = "a"
}

b = exec {
  command = a.path
  output = "b"
}

output result = b.path
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        // b depends on a (both namespaced)
        let order = dag.topo_order().unwrap();
        let a_pos = order.iter().position(|n| n == "inst.a").unwrap();
        let b_pos = order.iter().position(|n| n == "inst.b").unwrap();
        assert!(a_pos < b_pos);

        // b's command field references inst.a (namespaced)
        let node = dag.get_node("inst.b").unwrap();
        let cmd_field = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd_field.value,
            crate::ast::Expr::Ref(vec!["inst.a".into(), "path".into()])
        );
    }

    #[test]
    fn load_module_registers_targets() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
a = exec {
  command = "build"
  output = "a"
}

target build = [a]
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        let targets = dag.targets();
        assert!(targets.contains_key("inst.build"));
        let order = dag.target_order("inst.build").unwrap();
        assert!(order.contains(&"inst.a".to_owned()));
    }

    #[test]
    fn load_module_missing_param_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param required_val : string

a = exec {
  command = required_val
  output = "a"
}
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path());
        assert!(matches!(result, Err(LoadError::MissingParam { .. })));
    }

    #[test]
    fn load_module_default_param() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param cmd = "default_cmd"

a = exec {
  command = cmd
  output = "a"
}
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        let node = dag.get_node("inst.a").unwrap();
        let cmd_field = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd_field.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("default_cmd".into())])
        );
    }

    #[test]
    fn load_module_let_bindings() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param prefix : string

let full_cmd = "${prefix} world"

a = exec {
  command = full_cmd
  output = "a"
}
"#,
        );

        let input = r#"
inst = mymod {
  prefix = "hello"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        let node = dag.get_node("inst.a").unwrap();
        let cmd_field = node.fields.iter().find(|f| f.name == "command").unwrap();
        // Let binding evaluated: "hello world"
        assert_eq!(
            cmd_field.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("hello world".into())])
        );
    }

    #[test]
    fn load_multiple_module_instances() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param cmd : string

a = exec {
  command = cmd
  output = "a"
}
"#,
        );

        let input = r#"
inst1 = mymod {
  cmd = "echo one"
}
inst2 = mymod {
  cmd = "echo two"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        // Both instances create independent blocks
        assert!(dag.has_block("inst1.a"));
        assert!(dag.has_block("inst2.a"));
        assert!(dag.has_block("inst1"));
        assert!(dag.has_block("inst2"));
    }

    #[test]
    fn load_module_outer_block_dep() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param dep_path : string

a = exec {
  command = dep_path
  output = "a"
}

output result = a.path
"#,
        );

        // The module param references an outer block's output
        let input = r#"
outer = exec {
  command = "build outer"
  output = "outer"
}
inst = mymod {
  dep_path = outer.path
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        // inst.a should depend on outer (via the deferred param expression)
        let order = dag.topo_order().unwrap();
        let outer_pos = order.iter().position(|n| n == "outer").unwrap();
        let inner_pos = order.iter().position(|n| n == "inst.a").unwrap();
        assert!(outer_pos < inner_pos);
    }

    #[test]
    fn load_module_provider_resource_syntax() {
        let dir = tempfile::tempdir().unwrap();
        // Module at .bit/modules/myns/myres.bit (provider.resource syntax)
        write_module(
            dir.path(),
            "myns",
            "myres",
            r#"
a = exec {
  command = "build"
  output = "a"
}
"#,
        );

        let input = r#"
inst = myns.myres {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, dir.path()).unwrap();

        assert!(dag.has_block("inst.a"));
        assert!(dag.has_block("inst"));
    }

    #[test]
    fn matrix_expands_blocks() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build ${arch}"
  output = "out-${arch}"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();

        assert!(dag.has_block("build[amd64]"));
        assert!(dag.has_block("build[arm64]"));
        assert!(!dag.has_block("build"));
    }

    #[test]
    fn matrix_substitutes_key_in_fields() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = arch
  output = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();

        let node = dag.get_node("build[amd64]").unwrap();
        let cmd = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("amd64".into())])
        );
    }

    #[test]
    fn matrix_cross_block_refs() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build ${arch}"
  output = "out-${arch}"
}

deploy[arch] = exec {
  command = build.path
  output = "deploy-${arch}"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();

        // deploy[amd64] should depend on build[amd64], not build[arm64]
        let order = dag.topo_order().unwrap();
        let build_amd64 = order.iter().position(|n| n == "build[amd64]").unwrap();
        let deploy_amd64 = order.iter().position(|n| n == "deploy[amd64]").unwrap();
        assert!(build_amd64 < deploy_amd64);

        // deploy[amd64]'s command should reference build[amd64].path
        let node = dag.get_node("deploy[amd64]").unwrap();
        let cmd = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd.value,
            crate::ast::Expr::Ref(vec!["build[amd64]".into(), "path".into()])
        );
    }

    #[test]
    fn matrix_cartesian_product() {
        let input = r#"
let arch = ["amd64", "arm64"]
let os = ["linux", "darwin"]

build[arch, os] = exec {
  command = "build ${os}-${arch}"
  output = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();

        assert!(dag.has_block("build[amd64, linux]"));
        assert!(dag.has_block("build[amd64, darwin]"));
        assert!(dag.has_block("build[arm64, linux]"));
        assert!(dag.has_block("build[arm64, darwin]"));
    }

    #[test]
    fn matrix_non_matrix_depends_on_all_slices() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build ${arch}"
  output = "out-${arch}"
}

package = exec {
  command = "package"
  output = "pkg"
  depends_on = [build]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new(".")).unwrap();

        // package should come after both build slices
        let order = dag.topo_order().unwrap();
        let build_amd64 = order.iter().position(|n| n == "build[amd64]").unwrap();
        let build_arm64 = order.iter().position(|n| n == "build[arm64]").unwrap();
        let pkg = order.iter().position(|n| n == "package").unwrap();
        assert!(build_amd64 < pkg);
        assert!(build_arm64 < pkg);
    }
}
