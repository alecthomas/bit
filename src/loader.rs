use crate::ast::{Module, Statement};
use crate::dag::{Dag, DagError, DagNode, collect_after, collect_block_refs, collect_depends_on};
use crate::expr::{self, EvalError, Scope};
use crate::provider::ProviderRegistry;
use crate::state::{StateError, StateStore};
use crate::value::{Map, Value};

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
                scope.set(&p.name, value);
            }
            Statement::Let(l) => {
                let value = expr::eval(&l.value, &scope)?;
                scope.set(&l.name, value);
            }
            Statement::Block(b) => {
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

                // Register block name in scope as placeholder for non-dotted refs
                scope.set(&b.name, Value::Map(Map::new()));
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
        let module = parser::parse(input).unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
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
        let module = parser::parse(input).unwrap();
        let (dag, scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
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
        let module = parser::parse(input).unwrap();
        let mut params = Map::new();
        params.insert("env".into(), Value::Str("prod".into()));
        let (_dag, scope) = load(&module, &params, &test_registry(), &EmptyStore).unwrap();
        assert_eq!(scope.scope.get("env").unwrap().as_str(), Some("prod"));
    }

    #[test]
    fn load_missing_param() {
        let input = "param env : string\n";
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore);
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
        let module = parser::parse(input).unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
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
        let module = parser::parse(input).unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
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
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore);
        assert!(matches!(result, Err(LoadError::Dag(DagError::Cycle))));
    }

    #[test]
    fn load_unknown_provider() {
        let input = r#"
server = go.binary {
  main = "./cmd/server"
}
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore);
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
        let module = parser::parse(input).unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
        let order = dag.topo_order().unwrap();
        let mi = order.iter().position(|n| n == "migrations").unwrap();
        let di = order.iter().position(|n| n == "deploy").unwrap();
        assert!(mi < di);
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
        let module = parser::parse(input).unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore).unwrap();
        let targets = dag.targets();
        assert_eq!(targets["build"].doc.as_deref(), Some("Build the server"));
    }
}
