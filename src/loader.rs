use std::path::Path;

use crate::ast::{Module, Statement};
use crate::dag::{Dag, DagError, DagNode, collect_after, collect_block_refs, collect_depends_on};
use crate::expr::{self, EvalError, Scope};
use crate::module;
use crate::provider::ProviderRegistry;
use crate::state::{StateError, StateStore};
use crate::value::{Map, Value, validate_type};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Eval(#[from] EvalError),
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("{0}")]
    State(#[from] StateError),
    #[error("missing required param: {0}")]
    MissingParam(String),
    #[error("unknown provider/resource: {0}.{1}")]
    UnknownResource(String, String),
    #[error("{name}: {message}")]
    TypeError { name: String, message: String },
    #[error("failed to load module {0}: {1}")]
    ModuleLoad(String, String),
    #[error("failed to parse module {0}: {1}")]
    ModuleParse(String, String),
}

/// The base scope of evaluated params and let bindings, shared with the engine
/// for deferred field evaluation.
pub struct BaseScope {
    pub scope: Scope,
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

    for stmt in &module.statements {
        match stmt {
            Statement::Param(p) => {
                let value = if let Some(v) = params.get(&p.name) {
                    v.clone()
                } else if let Some(default) = &p.default {
                    expr::eval(default, &scope)?
                } else {
                    return Err(LoadError::MissingParam(p.name.clone()));
                };
                validate_type(&value, &p.typ).map_err(|message| LoadError::TypeError {
                    name: p.name.clone(),
                    message,
                })?;
                scope.set(&p.name, value);
            }
            Statement::Let(l) => {
                let value = expr::eval(&l.value, &scope)?;
                if let Some(typ) = &l.typ {
                    validate_type(&value, typ).map_err(|message| LoadError::TypeError {
                        name: l.name.clone(),
                        message,
                    })?;
                }
                scope.set(&l.name, value);
            }
            Statement::Block(b) => {
                // Check for module in .bit/modules/ before the provider registry
                if let Some(module_path) = module::resolve_module_path(root, &b.provider, &b.resource) {
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
                    let resource = registry
                        .get_resource(&b.provider, &b.resource)
                        .ok_or_else(|| LoadError::UnknownResource(b.provider.clone(), b.resource.clone()))?;

                    let prior_state = store.load(&b.name)?;

                    dag.add_node(DagNode {
                        name: b.name.clone(),
                        provider: b.provider.clone(),
                        resource_name: b.resource.clone(),
                        protected: b.protected,
                        fields: b.fields.clone(),
                        resource,
                        prior_state,
                    })?;

                    block_names.push(b.name.clone());

                    // Register block name in scope as placeholder
                    scope.set(&b.name, Value::Map(Map::new()));
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

    // Build dependency edges from field refs, depends_on, and after
    for stmt in &module.statements {
        if let Statement::Block(b) = stmt {
            // Implicit deps from expression refs (content-coupled)
            let refs = collect_block_refs(&b.fields);
            for dep in &refs {
                if dag.has_block(dep) && dep != &b.name {
                    dag.add_dep_edge(dep, &b.name)?;
                }
            }
            // Explicit depends_on (content-coupled)
            for dep in collect_depends_on(&b.fields) {
                if dag.has_block(&dep) && dep != b.name {
                    dag.add_dep_edge(&dep, &b.name)?;
                }
            }
            // Explicit after (ordering-only)
            for dep in collect_after(&b.fields) {
                if dag.has_block(&dep) && dep != b.name {
                    dag.add_ordering_edge(&dep, &b.name)?;
                }
            }
        }
    }

    dag.validate()?;

    Ok((dag, BaseScope { scope }))
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
    fn load_missing_param() {
        let input = "param env : string\n";
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, Path::new("."));
        assert!(matches!(result, Err(LoadError::MissingParam(_))));
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
        assert!(matches!(result, Err(LoadError::UnknownResource(..))));
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
        assert!(matches!(result, Err(LoadError::MissingParam(_))));
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
}
