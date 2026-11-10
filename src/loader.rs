use std::collections::HashMap;

use crate::ast::{Module, Statement};
use crate::dag::{
    AFTER_FIELD, DEPENDS_ON_FIELD, Dag, DagError, DagNode, collect_all_refs, collect_dependency_refs,
    collect_target_refs,
};
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
    #[error("{pos}: undefined '{name}' referenced in target '{from}'")]
    UnknownTarget {
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
    #[error("{pos}: target '{target}': {source}")]
    InvalidInputs {
        pos: crate::ast::Pos,
        target: String,
        source: Box<crate::input_check::InputError>,
    },
    #[error("{pos}: target '{target}' is {target_phase}, but its module instance '{instance}' is {instance_phase}")]
    PhaseConflict {
        pos: crate::ast::Pos,
        target: String,
        target_phase: crate::ast::Phase,
        instance: String,
        instance_phase: crate::ast::Phase,
    },
    #[error("{pos}: invalid invocation of '{name}': {message}")]
    InvalidInvocation {
        pos: crate::ast::Pos,
        name: String,
        message: String,
    },
}

/// The base scope of evaluated params and let bindings, shared with the engine
/// for deferred field evaluation.
pub struct BaseScope {
    pub scope: Scope,
    /// Params that were declared without defaults and not provided via -p,
    /// plus let bindings that transitively depend on them. Active targets
    /// referencing these will produce errors at execution time.
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
/// time because they may reference upstream target outputs.
pub fn load(
    module: &Module,
    params: &Map,
    registry: &ProviderRegistry,
    store: &dyn StateStore,
    import_roots: &[crate::import::ImportRoot],
) -> Result<(Dag, BaseScope), LoadError> {
    let (dag, base, _) = load_selected(module, params, registry, store, import_roots, &[])?;
    Ok((dag, base))
}

/// Load the graph and bind parameters for explicitly selected groups or targets.
pub fn load_selected(
    module: &Module,
    params: &Map,
    registry: &ProviderRegistry,
    store: &dyn StateStore,
    import_roots: &[crate::import::ImportRoot],
    invocations: &[crate::invocation::Invocation],
) -> Result<(Dag, BaseScope, Vec<String>), LoadError> {
    validate_names(module)?;
    let mut scope = Scope::with_providers(registry.clone());
    let mut dag = Dag::new();
    let mut concrete_targets = Vec::new();
    let mut matrix_targets: HashMap<String, Vec<String>> = HashMap::new();
    let mut deferred_matrix: Vec<crate::ast::Target> = Vec::new();
    let mut missing_params: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut declared_params: std::collections::HashSet<String> = std::collections::HashSet::new();

    for stmt in &module.statements {
        match stmt {
            Statement::Import(_) => {}
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
                    Err(EvalError::UndefinedVar(name)) if missing_params.contains(&name) => {
                        // Let depends on a missing param or an earlier deferred let.
                        missing_params.insert(l.name.clone());
                    }
                    Err(source) => {
                        return Err(LoadError::Eval {
                            pos: l.pos.clone(),
                            source,
                        });
                    }
                }
            }
            Statement::Target(b) => {
                if !b.params.is_empty() {
                    // A parameterized target is a declaration, not a graph node.
                    continue;
                }
                concrete_targets.push(b.clone());
                if !b.matrix_keys.is_empty() {
                    // Defer matrix targets — full define happens in expand_matrix.
                    // Check here only for conflicts with params/lets.
                    if let Some(existing) = scope.kind(&b.name) {
                        return Err(LoadError::DuplicateName {
                            pos: b.pos.clone(),
                            name: b.name.clone(),
                            existing: existing.as_str(),
                        });
                    }
                    matrix_targets.insert(b.name.clone(), b.matrix_key_names());
                    deferred_matrix.push(b.clone());
                } else if let Some(module_path) = module::resolve_module_path(import_roots, &b.provider, &b.resource) {
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
                        import_roots,
                    };
                    module::expand_module(
                        &b.name,
                        &module_path,
                        &b.fields,
                        module::InstanceModifiers::of(b),
                        &mut ctx,
                    )?;
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
                        explicit: b.explicit,
                        large: b.large,
                        concurrency_group: b.name.clone(),
                        fields: b.fields.clone(),
                        resource,
                        prior_state,
                    })?;

                    scope
                        .define(&b.name, expr::SymbolKind::Target, Value::strct(Map::new()))
                        .map_err(|existing| LoadError::DuplicateName {
                            pos: b.pos.clone(),
                            name: b.name.clone(),
                            existing: existing.as_str(),
                        })?;
                }
            }
            Statement::Group(_) => {}
            Statement::Output(_) => {
                // Outputs are deferred — they reference target outputs
                // which aren't available until execution.
            }
        }
    }

    // Expand matrix targets now that all params/lets are in scope
    for target in &deferred_matrix {
        matrix::expand_matrix(target, "", &mut scope, registry, store, &mut dag, &matrix_targets)?;
    }

    let selected_names = crate::invocation::materialize(
        module,
        invocations,
        &mut concrete_targets,
        &mut matrix_targets,
        &mut module::ExpandContext {
            scope: &mut scope,
            registry,
            store,
            dag: &mut dag,
            import_roots,
        },
    )?;

    // Include every actual DAG node so dependencies introduced by target calls
    // inside matrix slices and expanded modules are wired too. Source targets
    // remain in the list for module-level depends_on/after handling.
    let mut wired_targets: Vec<(String, crate::ast::Pos, Vec<crate::ast::Field>, bool)> = dag
        .target_names()
        .into_iter()
        .map(|name| {
            let node = dag.get_node(&name).expect("listed target exists");
            (name, node.pos.clone(), node.fields.clone(), false)
        })
        .collect();
    wired_targets.extend(
        concrete_targets
            .iter()
            .filter(|target| target.matrix_keys.is_empty())
            .map(|target| {
                (
                    target.name.clone(),
                    target.pos.clone(),
                    target.fields.clone(),
                    module::resolve_module_path(import_roots, &target.provider, &target.resource).is_some(),
                )
            }),
    );

    // Build dependency edges from field refs, depends_on, and after. Matrix
    // and module expansion may have wired some edges already; Dag deduplicates
    // them while this pass adds references created during materialization.
    for (target_name, target_pos, target_fields, is_module_source) in &wired_targets {
        let destinations = dependency_destinations(target_name, *is_module_source, &dag);
        // Implicit deps from expression refs — these may reference
        // scope variables (not targets), so we only create edges for
        // names that exist in the DAG or as matrix targets.
        let refs = collect_target_refs(target_fields, &scope).map_err(|source| LoadError::Eval {
            pos: target_pos.clone(),
            source,
        })?;
        for dep in &refs {
            if let Some(resolved) = resolve_dep(dep, &dag, &matrix_targets, &scope) {
                for r in &resolved {
                    for destination in &destinations {
                        if r != destination {
                            dag.add_dep_edge(r, destination)?;
                        }
                    }
                }
            }
        }
        // Explicit depends_on — must reference known targets.
        for dep in
            collect_dependency_refs(target_fields, DEPENDS_ON_FIELD, &scope).map_err(|source| LoadError::Eval {
                pos: target_pos.clone(),
                source,
            })?
        {
            let resolved =
                resolve_dep(&dep, &dag, &matrix_targets, &scope).ok_or_else(|| LoadError::UnknownTarget {
                    pos: target_pos.clone(),
                    name: dep.clone(),
                    from: target_name.clone(),
                })?;
            for r in &resolved {
                for destination in &destinations {
                    if r != destination {
                        dag.add_dep_edge(r, destination)?;
                    }
                }
            }
        }
        // Explicit after — must reference known targets.
        for dep in collect_dependency_refs(target_fields, AFTER_FIELD, &scope).map_err(|source| LoadError::Eval {
            pos: target_pos.clone(),
            source,
        })? {
            let resolved =
                resolve_dep(&dep, &dag, &matrix_targets, &scope).ok_or_else(|| LoadError::UnknownTarget {
                    pos: target_pos.clone(),
                    name: dep.clone(),
                    from: target_name.clone(),
                })?;
            for r in &resolved {
                for destination in &destinations {
                    if r != destination {
                        dag.add_ordering_edge(r, destination)?;
                    }
                }
            }
        }
    }

    // Validate that all expression refs in target fields resolve to known names.
    for (target_name, target_pos, target_fields, _) in &wired_targets {
        for name in collect_all_refs(target_fields) {
            if scope.get(&name).is_none()
                && !dag.has_target(&name)
                && !matrix_targets.contains_key(&name)
                && !missing_params.contains(&name)
            {
                return Err(LoadError::UnknownTarget {
                    pos: target_pos.clone(),
                    name,
                    from: target_name.clone(),
                });
            }
        }
    }

    crate::input_check::check_dag(&dag, &scope)?;

    dag.wire_phase_edges();
    dag.validate()?;

    for key in params.keys() {
        if !declared_params.contains(key) {
            return Err(LoadError::UnknownParam { name: key.clone() });
        }
    }

    Ok((dag, BaseScope { scope, missing_params }, selected_names))
}

/// Reject ambiguous top-level declarations before parameterized declarations
/// are collected into maps or deferred values can bypass scope checks.
pub(crate) fn validate_names(module: &Module) -> Result<(), LoadError> {
    let mut names = HashMap::new();
    for statement in &module.statements {
        let (name, pos, kind) = match statement {
            Statement::Param(param) => (&param.name, &param.pos, "param"),
            Statement::Let(binding) => (&binding.name, &binding.pos, "variable"),
            Statement::Target(target) => (&target.name, &target.pos, "target"),
            Statement::Group(group) => (&group.name, &group.pos, "group"),
            Statement::Import(_) | Statement::Output(_) => continue,
        };
        if let Some(existing) = names.insert(name.as_str(), kind) {
            return Err(LoadError::DuplicateName {
                pos: pos.clone(),
                name: name.clone(),
                existing,
            });
        }
    }
    Ok(())
}

fn dependency_destinations(name: &str, include_module_children: bool, dag: &Dag) -> Vec<String> {
    if !include_module_children {
        return vec![name.to_owned()];
    }
    let prefix = format!("{name}.");
    dag.target_names()
        .into_iter()
        .filter(|candidate| candidate == name || candidate.starts_with(&prefix))
        .collect()
}

/// Resolve a dependency name to actual DAG node names.
/// If the name is a matrix target, returns all expanded slice names, including
/// an empty list for a matrix with no slices. Returns `None` for an unknown name.
fn resolve_dep(
    name: &str,
    dag: &Dag,
    matrix_targets: &HashMap<String, Vec<String>>,
    scope: &Scope,
) -> Option<Vec<String>> {
    if dag.has_target(name) {
        return Some(vec![name.to_owned()]);
    }
    if !matrix_targets.contains_key(name) {
        return None;
    }
    matrix::instance_slices(name, scope)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::file_tracker::FileTracker;
    use crate::parser;
    use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
    use crate::providers::exec::ExecProvider;

    fn test_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        let tracker = Arc::new(FileTracker::new());
        reg.register(Box::new(ExecProvider::new(tracker)));
        reg.register(Box::new(DependencyProvider));
        reg
    }

    struct DependencyProvider;

    impl Provider for DependencyProvider {
        fn name(&self) -> &str {
            "dependency"
        }

        fn resources(&self) -> Vec<Box<dyn DynResource>> {
            Vec::new()
        }

        fn functions(&self) -> Vec<FuncSignature> {
            Vec::new()
        }

        fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
            if args.len() != 1 {
                return Err("unexpected dependency function call".into());
            }
            let package = args[0].as_str().ok_or("package must be a string")?;
            if name == "fail" {
                return Err("dependency function failed".into());
            }
            if name == "strings" {
                return Ok(Value::List(
                    crate::value::Type::String,
                    vec![Value::Str(r#"crate["core"]"#.into())],
                ));
            }
            if name != "of" {
                return Err("unexpected dependency function call".into());
            }
            let references = if package == "app" {
                vec![Value::TargetRef(r#"crate["core"]"#.into())]
            } else {
                Vec::new()
            };
            Ok(Value::List(crate::value::Type::TargetRef, references))
        }
    }

    struct EmptyStore;
    impl StateStore for EmptyStore {
        fn load(&self, _target: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(None)
        }
        fn save(&self, _target: &str, _state: &serde_json::Value) -> Result<(), StateError> {
            Ok(())
        }
        fn remove(&self, _target: &str) -> Result<(), StateError> {
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(vec![])
        }
    }

    #[test]
    fn load_simple_target() {
        let input = r#"
server = exec {
  command = "go build"
  outputs = "server"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
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
  outputs = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        assert!(dag.get_node("server").is_some());
        assert_eq!(scope.scope.get("name").unwrap().as_str(), Some("hello"));
    }

    #[test]
    fn load_param_with_value() {
        let input = r#"
param env : string
server = exec {
  command = env
  outputs = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let mut params = Map::new();
        params.insert("env".into(), Value::Str("prod".into()));
        let (_dag, scope) = load(&module, &params, &test_registry(), &EmptyStore, &[]).unwrap();
        assert_eq!(scope.scope.get("env").unwrap().as_str(), Some("prod"));
    }

    #[test]
    fn load_heterogeneous_map_param_default() {
        let module = parser::parse(
            r#"param person = {name: string = "Bob", age:int = null, enabled = true, unset = null}"#,
            "<test>",
        )
        .unwrap();
        let (_, scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let person = scope.scope.get("person").unwrap();
        let fields = person.as_map().unwrap();
        assert_eq!(fields.get("name").and_then(Value::as_str), Some("Bob"));
        assert_eq!(fields.get("age"), Some(&Value::Null));
        assert_eq!(fields.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(fields.get("unset"), Some(&Value::Null));
    }

    #[test]
    fn load_rejects_mismatched_map_field_annotation() {
        let module = parser::parse(r#"let person = {age: int = "Bob"}"#, "<test>").unwrap();
        let error = match load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]) {
            Err(error) => error,
            Ok(_) => panic!("expected an annotation mismatch"),
        };
        assert!(
            error
                .to_string()
                .contains("map field 'age': expected number, got string")
        );
    }

    #[test]
    fn load_missing_param_deferred() {
        // Missing params don't error at load time — they're deferred
        let input = "param env : string\n";
        let module = parser::parse(input, "<test>").unwrap();
        let (_dag, base) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        assert!(base.missing_params.contains("env"));
    }

    #[test]
    fn load_let_depending_on_missing_param_is_deferred() {
        let input = r##"param env : string
let prefix = env
let command = "#{prefix} echo"
"##;
        let module = parser::parse(input, "<test>").unwrap();
        let (_dag, base) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert!(base.missing_params.contains("env"));
        assert!(base.missing_params.contains("prefix"));
        assert!(base.missing_params.contains("command"));
        assert!(base.scope.get("prefix").is_none());
        assert!(base.scope.get("command").is_none());
    }

    #[test]
    fn load_provider_failure_in_let_is_reported_at_declaration() {
        let input = r#"let package = dependency.fail("package")

crate[package] = exec {
  command = "build #{package}"
  outputs = "out-#{package}"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);

        match result {
            Err(LoadError::Eval {
                pos,
                source: EvalError::ProviderFunction { name, message },
            }) => {
                assert_eq!(
                    pos,
                    crate::ast::Pos {
                        file: "<test>".into(),
                        line: 1,
                        col: 1,
                    }
                );
                assert_eq!(name, "dependency.fail");
                assert_eq!(message, "dependency function failed");
            }
            _ => panic!("expected provider function evaluation error"),
        }
    }

    #[test]
    fn load_builtin_failure_in_let_is_reported_at_declaration() {
        let input = "let value = trim()\n";
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);

        match result {
            Err(LoadError::Eval {
                pos,
                source: EvalError::Arity { name, expected, got },
            }) => {
                assert_eq!(
                    pos,
                    crate::ast::Pos {
                        file: "<test>".into(),
                        line: 1,
                        col: 1,
                    }
                );
                assert_eq!(name, "trim");
                assert_eq!(expected, 1);
                assert_eq!(got, 0);
            }
            _ => panic!("expected built-in function evaluation error"),
        }
    }

    #[test]
    fn load_unknown_param_errors() {
        let input = "param env : string\n";
        let module = parser::parse(input, "<test>").unwrap();
        let mut params = Map::new();
        params.insert("bogus".into(), Value::Str("val".into()));
        let result = load(&module, &params, &test_registry(), &EmptyStore, &[]);
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("unknown param: bogus"), "got: {err}");
    }

    #[test]
    fn load_dependency_edges() {
        let input = r#"
a = exec {
  command = "build a"
  outputs = "a"
}
b = exec {
  command = a.path
  outputs = "b"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let order = dag.topo_order().unwrap();
        let ai = order.iter().position(|n| n == "a").unwrap();
        let bi = order.iter().position(|n| n == "b").unwrap();
        assert!(ai < bi);
    }

    #[test]
    fn topo_order_is_stable_and_alphabetical() {
        // Three independent targets defined out of alphabetical order.
        // Stable order should be alphabetical regardless of input order.
        let input = r#"
zebra = exec { command = "z" outputs = "z" }
apple = exec { command = "a" outputs = "a" }
mango = exec { command = "m" outputs = "m" }
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        assert_eq!(dag.topo_order().unwrap(), vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn primary_parent_prefers_dependency_over_ordering() {
        // integration-test content-depends on debug and has a phase-edge
        // ordering-dep on fmt. Primary should be debug.
        let input = r#"
pre fmt = exec { command = "fmt" outputs = "fmt" }
debug = exec { command = "d" outputs = "d" }
integration-test = exec {
  command  = "t"
  outputs   = "t"
  depends_on = [debug]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        assert_eq!(dag.primary_parent("integration-test"), Some("debug".into()));
        assert_eq!(dag.primary_parent("debug"), Some("fmt".into()));
        assert_eq!(dag.primary_parent("fmt"), None);
    }

    #[test]
    fn load_group() {
        let input = r#"
server = exec {
  command = "build"
  outputs = "out"
}
group build = [server]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let order = dag.selection_order("build").unwrap();
        assert_eq!(order, vec!["server"]);
    }

    #[test]
    fn duplicate_declaration_names_are_rejected() {
        let cases = [
            (
                "clippy = exec { command = \"lint\" outputs = \"out\" }\ngroup clippy = [clippy]\n",
                "target",
            ),
            (
                "group clippy = [clippy]\nclippy = exec { command = \"lint\" outputs = \"out\" }\n",
                "group",
            ),
            ("group clippy = []\ngroup clippy = []\n", "group"),
            (
                "clippy(value : string) = exec { command = value outputs = \"out\" }\ngroup clippy = []\n",
                "target",
            ),
            ("param clippy : string\ngroup clippy = []\n", "param"),
        ];
        for (input, existing) in cases {
            let module = parser::parse(input, "<test>").unwrap();
            let error = match load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]) {
                Err(error) => error,
                Ok(_) => panic!("expected duplicate name error for {input}"),
            };
            assert_eq!(
                error.to_string(),
                format!("<test>:2:1: name 'clippy' is already used by a {existing}")
            );
        }
    }

    #[test]
    fn duplicate_declaration_names_in_modules_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            "clippy = exec { command = \"lint\" outputs = \"out\" }\ngroup clippy = [clippy]\n",
        );
        let module = parser::parse("inst = mymod {}\n", "<test>").unwrap();
        let error = match load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        ) {
            Err(error) => error,
            Ok(_) => panic!("expected duplicate name error"),
        };
        assert!(matches!(
            error,
            LoadError::DuplicateName { name, existing: "target", pos }
                if name == "clippy" && pos.line == 2 && pos.col == 1
        ));
    }

    #[test]
    fn load_parameterized_group_materializes_bound_target() {
        let input = r#"
image(tag : string) = exec {
  command = "build #{tag}"
  outputs = "out"
}
group publish(tag : string) = [image(tag = tag)]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "publish".into(),
            args: vec![("tag".into(), "v1".into())],
        }];
        let (dag, _scope, selected) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        assert_eq!(selected, vec![r#"publish(tag="v1")"#]);
        assert_eq!(dag.selection_order(&selected[0]).unwrap(), vec![r#"image(tag="v1")"#]);
        let node = dag.get_node(r#"image(tag="v1")"#).unwrap();
        assert_eq!(
            node.fields.iter().find(|field| field.name == "command").unwrap().value,
            crate::ast::Expr::Str(vec![
                crate::ast::StringPart::Literal("build ".into()),
                crate::ast::StringPart::Interpolation(crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal(
                    "v1".into()
                ),])),
            ])
        );
    }

    #[test]
    fn secrets_cannot_be_bound_to_parameters() {
        // SAFETY: only this test reads or writes this variable.
        unsafe { std::env::set_var("BIT_TEST_LOADER_SECRET", "hunter2") };
        let cases = [
            (
                "argument",
                "job(token : string) = exec { command = token outputs = \"out\" }\n\
                 group run = [job(token = secret(\"BIT_TEST_LOADER_SECRET\"))]\n",
            ),
            (
                "nested argument",
                "job(tokens : [string]) = exec { command = \"x\" outputs = \"out\" }\n\
                 group run = [job(tokens = [secret(\"BIT_TEST_LOADER_SECRET\")])]\n",
            ),
            (
                "default",
                "job(token = secret(\"BIT_TEST_LOADER_SECRET\")) = exec { command = token outputs = \"out\" }\n\
                 group run = [job()]\n",
            ),
            (
                "group argument",
                "job(token : string) = exec { command = token outputs = \"out\" }\n\
                 group run(token : string) = [job(token = token)]\n\
                 group outer = [run(token = secret(\"BIT_TEST_LOADER_SECRET\"))]\n",
            ),
        ];
        for (case, input) in cases {
            let module = parser::parse(input, "<test>").unwrap();
            let error = match load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &[]) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("{case}: a secret argument must be rejected"),
            };
            assert!(error.contains("cannot receive a secret"), "{case}: {error}");
            assert!(!error.contains("hunter2"), "{case}: {error}");
        }

        // CLI arguments are parsed as expressions for non-string parameters.
        let module = parser::parse(
            "job(tokens : [string]) = exec { command = \"x\" outputs = \"out\" }\n",
            "<test>",
        )
        .unwrap();
        let invocation = crate::invocation::Invocation {
            name: "job".into(),
            args: vec![("tokens".into(), r#"[secret("BIT_TEST_LOADER_SECRET")]"#.into())],
        };
        assert!(load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &[invocation]).is_err());
    }

    #[test]
    fn a_target_may_read_a_secret_in_its_body() {
        // SAFETY: only this test reads or writes this variable.
        unsafe { std::env::set_var("BIT_TEST_LOADER_BODY_SECRET", "hunter2") };
        let input = "let token = secret(\"BIT_TEST_LOADER_BODY_SECRET\")\n\
                     job(name : string) = exec { command = token outputs = \"out\" }\n\
                     group run = [job(name = \"app\")]\n";
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, scope, _) = load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &[]).unwrap();
        assert_eq!(
            scope.scope.get("token"),
            Some(&Value::Secret {
                key: "BIT_TEST_LOADER_BODY_SECRET".into(),
                value: "hunter2".into(),
            })
        );
        assert!(dag.has_target(r#"job(name="app")"#));
    }

    #[test]
    fn parameterized_invocations_share_equal_arguments_and_separate_different_arguments() {
        let input = r#"
job(value : string) = exec { command = value outputs = "out" }
group first(value : string) = [job(value = value)]
group second(value : string) = [job(value = value)]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [
            crate::invocation::Invocation {
                name: "first".into(),
                args: vec![("value".into(), "a".into())],
            },
            crate::invocation::Invocation {
                name: "second".into(),
                args: vec![("value".into(), "a".into())],
            },
            crate::invocation::Invocation {
                name: "second".into(),
                args: vec![("value".into(), "b".into())],
            },
        ];
        let (dag, _scope, selected) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        assert_eq!(selected.len(), 3);
        assert!(dag.has_target(r#"job(value="a")"#));
        assert!(dag.has_target(r#"job(value="b")"#));
        assert_eq!(dag.target_names().len(), 2);
    }

    #[test]
    fn target_references_materialize_parameterized_targets_in_fields_and_dependencies() {
        let input = r#"
prepare(package : string) = exec {
  command = "prepare #{package}"
  outputs = "prepared"
}
compile(package : string) = exec {
  command = prepare(package = package).path
  outputs = "compiled"
  depends_on = [prepare(package = package)]
}
release(package : string) = exec {
  command = compile(package = package).path
  outputs = "released"
  after = [prepare(package = package)]
}
group publish(package : string) = [release(package = package)]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "publish".into(),
            args: vec![("package".into(), "api".into())],
        }];
        let (dag, _scope, _) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        let prepare = r#"prepare(package="api")"#;
        let compile = r#"compile(package="api")"#;
        let release = r#"release(package="api")"#;
        assert!(dag.has_target(prepare));
        assert!(dag.has_target(compile));
        assert!(dag.has_target(release));
        assert_eq!(dag.target_names().len(), 3);
        assert!(dag.content_deps(compile).contains(&prepare.to_owned()));
        assert!(dag.content_deps(release).contains(&compile.to_owned()));
        assert!(dag.deps(release).contains(&prepare.to_owned()));
        assert!(!dag.content_deps(release).contains(&prepare.to_owned()));
    }

    #[test]
    fn matrix_dependencies_can_invoke_parameterized_targets() {
        let input = r#"
let arch = ["amd64", "arm64"]

prepare(arch : string) = exec {
  command = "prepare #{arch}"
  outputs = "prepared"
}

build[arch] = exec {
  command = "build #{arch}"
  outputs = "built"
  depends_on = [prepare(arch = arch)]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        for arch in ["amd64", "arm64"] {
            let prepare = format!(r#"prepare(arch="{arch}")"#);
            let build = format!(r#"build["{arch}"]"#);
            assert!(dag.has_target(&prepare));
            assert!(dag.content_deps(&build).contains(&prepare));
        }
    }

    #[test]
    fn parameterized_matrix_calls_select_slices_and_share_instances() {
        let input = r#"
let arch = ["amd64", "arm64"]

prepare[arch](tag : string) = exec {
  command = "prepare #{arch} #{tag}"
  outputs = "prepared"
}
build[arch](tag : string) = exec {
  command = prepare[arch](tag = tag).path
  outputs = "built"
  depends_on = [prepare[arch](tag = tag)]
}
group release(tag : string) = [build(tag = tag)]
group arm(tag : string) = [build["arm64"](tag = tag)]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [
            crate::invocation::Invocation {
                name: "release".into(),
                args: vec![("tag".into(), "1.2.3".into())],
            },
            crate::invocation::Invocation {
                name: "arm".into(),
                args: vec![("tag".into(), "1.2.3".into())],
            },
            crate::invocation::Invocation {
                name: "arm".into(),
                args: vec![("tag".into(), "2.0.0".into())],
            },
        ];
        let (dag, _scope, selected) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        for tag in ["1.2.3", "2.0.0"] {
            for arch in ["amd64", "arm64"] {
                let build = format!(r#"build["{arch}"](tag="{tag}")"#);
                let prepare = format!(r#"prepare["{arch}"](tag="{tag}")"#);
                assert!(dag.has_target(&build));
                assert!(dag.content_deps(&build).contains(&prepare));
            }
        }
        assert_eq!(dag.target_names().len(), 8);
        assert_eq!(dag.groups()[&selected[0]].targets.len(), 2);
        assert_eq!(
            dag.groups()[&selected[1]].targets,
            vec![r#"build["arm64"](tag="1.2.3")"#]
        );
        assert_eq!(
            dag.groups()[&selected[2]].targets,
            vec![r#"build["arm64"](tag="2.0.0")"#]
        );
    }

    #[test]
    fn parameterized_matrix_call_in_dependencies_selects_every_slice() {
        let input = r#"
let arch = ["amd64", "arm64"]

image[arch](tag = "dev") = exec {
  command = "image #{arch} #{tag}"
  outputs = "image"
}
push(tag : string) = exec {
  command = "push #{tag}"
  outputs = "pushed"
  depends_on = [image(tag = tag)]
}
announce(tag : string) = exec {
  command = "announce #{tag}"
  outputs = "announced"
  after = [image(tag = tag)]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = ["push", "announce"].map(|name| crate::invocation::Invocation {
            name: name.into(),
            args: vec![("tag".into(), "x".into())],
        });
        let (dag, _scope, _) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        let push = r#"push(tag="x")"#;
        let announce = r#"announce(tag="x")"#;
        for arch in ["amd64", "arm64"] {
            let image = format!(r#"image["{arch}"](tag="x")"#);
            assert!(dag.content_deps(push).contains(&image));
            assert!(dag.deps(announce).contains(&image));
            assert!(!dag.content_deps(announce).contains(&image));
        }
    }

    #[test]
    fn matrix_key_list_can_be_a_target_parameter() {
        let input = r#"
work[arch](arch : [string], tag : string) = exec {
  command = "build #{arch} #{tag}"
  outputs = "built"
}
group release = [work(arch = ["arm64"], tag = "1.2.3")]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let order = dag.selection_order("release").unwrap();
        assert_eq!(order.len(), 1);
        let node = dag.get_node(&order[0]).unwrap();
        let command = node.fields.iter().find(|field| field.name == "command").unwrap();
        assert_eq!(command.value.to_string(), r#""build #{"arm64"} #{"1.2.3"}""#);
    }

    #[test]
    fn matrix_key_values_depend_on_earlier_keys_and_params() {
        let input = r##"
let module = ["a", "b"]

test[module, package = split("#{module}/x,#{module}/#{suffix}", ",")](suffix : string) = exec {
  command = "test #{module} #{package}"
  outputs = "tested"
}
group all = [test(suffix = "y")]
"##;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "all".into(),
            args: vec![],
        }];
        let (dag, _scope, selected) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        assert_eq!(
            dag.groups()[&selected[0]].targets,
            vec![
                r#"test["a", "a/x"](suffix="y")"#,
                r#"test["a", "a/y"](suffix="y")"#,
                r#"test["b", "b/x"](suffix="y")"#,
                r#"test["b", "b/y"](suffix="y")"#,
            ]
        );
        let node = dag.get_node(r#"test["b", "b/y"](suffix="y")"#).unwrap();
        let command = node.fields.iter().find(|field| field.name == "command").unwrap();
        assert_eq!(command.value.to_string(), r#""test #{"b"} #{"b/y"}""#);
    }

    #[test]
    fn load_cycle_detected() {
        let input = r#"
a = exec {
  command = b.path
  outputs = "a"
}
b = exec {
  command = a.path
  outputs = "b"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);
        match result {
            Err(error) => assert_eq!(error.to_string(), "dependency cycle detected: a -> b -> a"),
            Ok(_) => panic!("expected dependency cycle"),
        }
    }

    #[test]
    fn load_unknown_provider() {
        let input = r#"
server = go.binary {
  main = "./cmd/server"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);
        assert!(matches!(result, Err(LoadError::UnknownResource { .. })));
    }

    #[test]
    fn load_depends_on_creates_edge() {
        let input = r#"
migrations = exec {
  command = "migrate"
  outputs = "m"
}
deploy = exec {
  command = "deploy"
  outputs = "d"
  depends_on = [migrations]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let order = dag.topo_order().unwrap();
        let mi = order.iter().position(|n| n == "migrations").unwrap();
        let di = order.iter().position(|n| n == "deploy").unwrap();
        assert!(mi < di);
    }

    #[test]
    fn load_group_unknown_target() {
        let input = "group build = [nonexistent]\n";
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);
        assert!(matches!(result, Err(LoadError::Dag(DagError::UnknownGroupMember(..)))));
    }

    #[test]
    fn load_group_doc() {
        let input = r#"
server = exec {
  command = "build"
  outputs = "out"
}
# Build the server
group build = [server]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
        let groups = dag.groups();
        assert_eq!(groups["build"].doc.as_deref(), Some("Build the server"));
    }

    /// Create a module file at {dir}/{provider}/{resource}.bit.
    fn write_module(dir: &std::path::Path, provider: &str, resource: &str, content: &str) {
        let module_dir = dir.join(provider);
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join(format!("{resource}.bit")), content).unwrap();
    }

    /// Build the `ImportRoot` vec for a single provider whose modules live
    /// under `{dir}/{provider}/`.
    fn roots(dir: &std::path::Path, provider: &str) -> Vec<crate::import::ImportRoot> {
        vec![crate::import::ImportRoot {
            provider: provider.into(),
            path: dir.join(provider),
        }]
    }

    #[test]
    fn load_module_creates_namespaced_targets() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param cmd : string

inner = exec {
  command = cmd
  outputs = "out"
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
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        // Inner target is namespaced
        assert!(dag.has_target("inst.inner"));
        // Module instance target exists
        assert!(dag.has_target("inst"));
        // Module instance depends on inner target
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
  outputs = "out"
}
"#,
        );

        let input = r#"
inst = mymod {
  greeting = "hi there"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        let node = dag.get_node("inst.hello").unwrap();
        // The command field should have the substituted literal value
        let cmd_field = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd_field.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("hi there".into())])
        );
    }

    #[test]
    fn load_module_inner_target_refs_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
a = exec {
  command = "build a"
  outputs = "a"
}

b = exec {
  command = a.path
  outputs = "b"
}

output result = b.path
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

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
    fn load_module_registers_groups() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
a = exec {
  command = "build"
  outputs = "a"
}

group build = [a]
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        let groups = dag.groups();
        assert!(groups.contains_key("inst.build"));
        let order = dag.selection_order("inst.build").unwrap();
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
  outputs = "a"
}
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        );
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
  outputs = "a"
}
"#,
        );

        let input = r#"
inst = mymod {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

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
            r##"
param prefix : string

let full_cmd = "#{prefix} world"

a = exec {
  command = full_cmd
  outputs = "a"
}
"##,
        );

        let input = r#"
inst = mymod {
  prefix = "hello"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

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
  outputs = "a"
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
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        // Both instances create independent targets
        assert!(dag.has_target("inst1.a"));
        assert!(dag.has_target("inst2.a"));
        assert!(dag.has_target("inst1"));
        assert!(dag.has_target("inst2"));
    }

    #[test]
    fn load_module_outer_target_dep() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param dep_path : string

a = exec {
  command = dep_path
  outputs = "a"
}

output result = a.path
"#,
        );

        // The module param references an outer target's output
        let input = r#"
outer = exec {
  command = "build outer"
  outputs = "outer"
}
inst = mymod {
  dep_path = outer.path
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        // inst.a should depend on outer (via the deferred param expression)
        let order = dag.topo_order().unwrap();
        let outer_pos = order.iter().position(|n| n == "outer").unwrap();
        let inner_pos = order.iter().position(|n| n == "inst.a").unwrap();
        assert!(outer_pos < inner_pos);
    }

    #[test]
    fn module_dependencies_can_invoke_parameterized_targets() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
a = exec {
  command = "build"
  outputs = "a"
}
"#,
        );

        let input = r#"
prepare(env : string) = exec {
  command = "prepare #{env}"
  outputs = "prepared"
}

inst = mymod {
  depends_on = [prepare(env = "prod")]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        let prepare = r#"prepare(env="prod")"#.to_owned();
        assert!(dag.has_target(&prepare));
        assert!(dag.content_deps("inst.a").contains(&prepare));
    }

    #[test]
    fn imported_module_targets_can_declare_and_pass_parameters() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
param package : string

compile(package : string) = exec {
  command = "compile #{package}"
  outputs = "compiled"
}

image = exec {
  command = compile(package = package).path
  outputs = "image"
}

output path = image.path
"#,
        );

        let module = parser::parse(r#"inst = mymod { package = "api" }"#, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        let compile = r#"inst.compile(package="api")"#.to_owned();
        assert!(dag.has_target(&compile));
        assert!(dag.content_deps("inst.image").contains(&compile));
    }

    #[test]
    fn large_module_instance_marks_every_expanded_target_large() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
compile(package : string) = exec {
  command = "compile #{package}"
  outputs = "compiled"
}

image = exec {
  command = compile(package = "api").path
  outputs = "image"
}

output path = image.path
"#,
        );

        let module = parser::parse("large heavy = mymod {}\nlight = mymod {}\n", "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        for (prefix, large) in [("heavy", true), ("light", false)] {
            for name in [
                prefix.to_owned(),
                format!("{prefix}.image"),
                format!(r#"{prefix}.compile(package="api")"#),
            ] {
                assert_eq!(dag.get_node(&name).unwrap().large, large, "{name}");
            }
        }
    }

    #[test]
    fn module_instance_modifiers_reach_every_expanded_target() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
compile(package : string) = exec {
  command = "compile #{package}"
  outputs = "compiled"
}

pre prepare = exec {
  command = "prepare"
  outputs = "prepared"
}

image = exec {
  command = compile(package = "api").path
  outputs = "image"
}

output path = image.path
"#,
        );

        let module = parser::parse("pre protected explicit setup = mymod {}\nplain = mymod {}\n", "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        for (prefix, modified) in [("setup", true), ("plain", false)] {
            for name in [
                prefix.to_owned(),
                format!("{prefix}.image"),
                format!("{prefix}.prepare"),
                format!(r#"{prefix}.compile(package="api")"#),
            ] {
                let node = dag.get_node(&name).unwrap();
                let phase = if modified || name.ends_with(".prepare") {
                    crate::ast::Phase::Pre
                } else {
                    crate::ast::Phase::Default
                };
                assert_eq!(node.phase, phase, "{name}");
                assert_eq!(node.protected, modified, "{name}");
                assert_eq!(node.explicit, modified, "{name}");
            }
        }
    }

    #[test]
    fn module_target_phase_must_not_contradict_instance_phase() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            "post report = exec {\n  command = \"report\"\n}\n",
        );

        let module = parser::parse("pre setup = mymod {}\n", "<test>").unwrap();
        let result = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        );

        assert!(matches!(result, Err(LoadError::PhaseConflict { .. })));
    }

    #[test]
    fn imported_module_groups_can_declare_parameters() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
job(value : string) = exec {
  command = value
  outputs = "out"
}

group run(value : string) = [job(value = value)]
"#,
        );

        let module = parser::parse("inst = mymod {}", "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "inst.run".into(),
            args: vec![("value".into(), "hello".into())],
        }];
        let (dag, _scope, selected) = load_selected(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
            &invocations,
        )
        .unwrap();

        assert_eq!(selected, vec![r#"inst.run(value="hello")"#]);
        assert_eq!(
            dag.selection_order(&selected[0]).unwrap(),
            vec![r#"inst.job(value="hello")"#]
        );
    }

    #[test]
    fn imported_module_group_selects_parameterized_matrix_slice() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            r#"
job[arch](arch : [string], tag : string) = exec {
  command = "build #{arch} #{tag}"
  outputs = "out"
}
group run = [job["arm64"](arch = ["amd64", "arm64"], tag = "v1")]
"#,
        );

        let module = parser::parse("inst = mymod {}", "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();

        assert_eq!(
            dag.selection_order("inst.run").unwrap(),
            vec![r#"inst.job["arm64"](arch=["amd64", "arm64"], tag="v1")"#]
        );
    }

    #[test]
    fn load_module_provider_resource_syntax() {
        let dir = tempfile::tempdir().unwrap();
        // Module file at {dir}/myns/myres.bit; the import root points at
        // {dir}/myns with provider name "myns" (one provider per import).
        write_module(
            dir.path(),
            "myns",
            "myres",
            r#"
a = exec {
  command = "build"
  outputs = "a"
}
"#,
        );

        let input = r#"
inst = myns.myres {}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "myns"),
        )
        .unwrap();

        assert!(dag.has_target("inst.a"));
        assert!(dag.has_target("inst"));
    }

    #[test]
    fn matrix_expands_targets() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build #{arch}"
  outputs = "out-#{arch}"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert!(dag.has_target(r#"build["amd64"]"#));
        assert!(dag.has_target(r#"build["arm64"]"#));
        assert!(!dag.has_target("build"));
    }

    #[test]
    fn large_modifier_reaches_plain_matrix_and_parameterized_nodes() {
        let input = r#"
let arch = ["amd64", "arm64"]
large plain = exec { command = "a" outputs = "a" }
small = exec { command = "b" outputs = "b" }
large build[arch] = exec { command = "build #{arch}" outputs = "out-#{arch}" }
large image(tag : string) = exec { command = "image #{tag}" outputs = "image" }
group publish = [image(tag = "v1")]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "publish".into(),
            args: vec![],
        }];
        let (dag, _scope, _selected) =
            load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations).unwrap();

        for (name, large) in [
            ("plain", true),
            ("small", false),
            (r#"build["amd64"]"#, true),
            (r#"build["arm64"]"#, true),
            (r#"image(tag="v1")"#, true),
        ] {
            assert_eq!(dag.get_node(name).unwrap().large, large, "{name}");
        }
    }

    #[test]
    fn matrix_dependencies_can_come_from_target_reference_function() {
        let input = r#"
let package = ["app", "core"]

crate[package] = exec {
  command = "build #{package}"
  outputs = "out-#{package}"
  depends_on = dependency.of(package)
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        let order = dag.topo_order().unwrap();
        let core = order.iter().position(|name| name == r#"crate["core"]"#).unwrap();
        let app = order.iter().position(|name| name == r#"crate["app"]"#).unwrap();
        assert!(core < app);
    }

    #[test]
    fn matrix_key_can_be_a_target_reference() {
        let input = r#"
let dependency = dependency.of("app")

consumer[dependency] = exec {
  command = "consume"
  outputs = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert!(dag.has_target(r#"consumer[crate["core"]]"#));
    }

    #[test]
    fn dependency_function_does_not_coerce_strings_to_target_references() {
        let input = r#"
core = exec {
  command = "build core"
  outputs = "out-core"
}

app = exec {
  command = "build app"
  outputs = "out-app"
  depends_on = dependency.strings("app")
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let result = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]);

        match result {
            Err(LoadError::Eval {
                source: EvalError::Type(message),
                ..
            }) => assert!(message.contains("list of target references")),
            _ => panic!("expected a target reference type error"),
        }
    }

    #[test]
    fn matrix_substitutes_key_in_fields() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = arch
  outputs = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        let node = dag.get_node(r#"build["amd64"]"#).unwrap();
        let cmd = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd.value,
            crate::ast::Expr::Str(vec![crate::ast::StringPart::Literal("amd64".into())])
        );
    }

    #[test]
    fn matrix_cross_target_refs() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build #{arch}"
  outputs = "out-#{arch}"
}

deploy[arch] = exec {
  command = build.path
  outputs = "deploy-#{arch}"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        // deploy["amd64"] should depend on build["amd64"], not build["arm64"]
        let order = dag.topo_order().unwrap();
        let build_amd64 = order.iter().position(|n| n == r#"build["amd64"]"#).unwrap();
        let deploy_amd64 = order.iter().position(|n| n == r#"deploy["amd64"]"#).unwrap();
        assert!(build_amd64 < deploy_amd64);

        // deploy["amd64"]'s command should reference build["amd64"].path
        let node = dag.get_node(r#"deploy["amd64"]"#).unwrap();
        let cmd = node.fields.iter().find(|f| f.name == "command").unwrap();
        assert_eq!(
            cmd.value,
            crate::ast::Expr::Ref(vec![r#"build["amd64"]"#.into(), "path".into()])
        );
    }

    #[test]
    fn matrix_slice_reference_uses_typed_key() {
        let input = r#"
let arch = ["amd64"]
let selected_arch = "amd64"

build[arch] = exec {
  command = "build"
  outputs = "out"
}

package = exec {
  command = build[selected_arch].path
  outputs = "package"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        let order = dag.topo_order().unwrap();
        let build = order.iter().position(|name| name == r#"build["amd64"]"#).unwrap();
        let package = order.iter().position(|name| name == "package").unwrap();
        assert!(build < package);
    }

    #[test]
    fn matrix_cartesian_product() {
        let input = r#"
let arch = ["amd64", "arm64"]
let os = ["linux", "darwin"]

build[arch, os] = exec {
  command = "build #{os}-#{arch}"
  outputs = "out"
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert!(dag.has_target(r#"build["amd64", "linux"]"#));
        assert!(dag.has_target(r#"build["amd64", "darwin"]"#));
        assert!(dag.has_target(r#"build["arm64", "linux"]"#));
        assert!(dag.has_target(r#"build["arm64", "darwin"]"#));
    }

    #[test]
    fn matrix_non_matrix_depends_on_all_slices() {
        let input = r#"
let arch = ["amd64", "arm64"]

build[arch] = exec {
  command = "build #{arch}"
  outputs = "out-#{arch}"
}

package = exec {
  command = "package"
  outputs = "pkg"
  depends_on = [build]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        // package should come after both build slices
        let order = dag.topo_order().unwrap();
        let build_amd64 = order.iter().position(|n| n == r#"build["amd64"]"#).unwrap();
        let build_arm64 = order.iter().position(|n| n == r#"build["arm64"]"#).unwrap();
        let pkg = order.iter().position(|n| n == "package").unwrap();
        assert!(build_amd64 < pkg);
        assert!(build_arm64 < pkg);
    }

    #[test]
    fn group_expands_matrix_target_to_all_slices() {
        let input = r#"
let package = ["api", "worker"]

tests[package] = exec.test {
  command = "test #{package}"
}

group check = [tests]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        let order = dag.selection_order("check").unwrap();
        assert_eq!(order, vec![r#"tests["api"]"#, r#"tests["worker"]"#]);
    }

    #[test]
    fn group_accepts_empty_matrix_target() {
        let input = r#"
let package = []

tests[package] = exec.test {
  command = "test #{package}"
}

group check = [tests]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert_eq!(dag.selection_order("check").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn group_accepts_dots_in_matrix_values() {
        let input = r#"
let package = ["github.com/block/spectre/internal"]

tests[package] = exec.test {
  command = "test #{package}"
}

group check = [tests]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let (dag, _scope) = load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();

        assert_eq!(
            dag.selection_order("check").unwrap(),
            vec![r#"tests["github.com/block/spectre/internal"]"#]
        );
    }

    fn select(input: &str, selectors: &[&str]) -> Result<Vec<String>, LoadError> {
        let module = parser::parse(input, "<test>").unwrap();
        let invocations: Vec<_> = selectors
            .iter()
            .map(|name| crate::invocation::Invocation {
                name: (*name).into(),
                args: Vec::new(),
            })
            .collect();
        load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations)
            .map(|(_dag, _scope, selected)| selected)
    }

    #[test]
    fn cli_matrix_selector_accepts_unquoted_strings() {
        let input = r#"
let arch = ["amd64", "aarch64-apple-darwin"]
let os = ["linux"]

build[arch, os] = exec { command = "build #{arch} #{os}" }
"#;
        assert_eq!(
            select(
                input,
                &[
                    "build[amd64, linux]",
                    r#"build["amd64",linux]"#,
                    "build[aarch64-apple-darwin, linux]"
                ]
            )
            .unwrap(),
            vec![
                r#"build["amd64", "linux"]"#,
                r#"build["amd64", "linux"]"#,
                r#"build["aarch64-apple-darwin", "linux"]"#,
            ]
        );
    }

    #[test]
    fn cli_matrix_selector_keeps_literal_keys_typed() {
        let input = r#"
let version = [1, "2", "1.2"]

build[version] = exec { command = "build #{version}" }
"#;
        assert_eq!(
            select(input, &["build[1]", "build[\"2\"]", "build[\"1.2\"]"]).unwrap(),
            vec!["build[1]", r#"build["2"]"#, r#"build["1.2"]"#]
        );
        // Only identifiers are bare strings; `1.2` stays a number and is
        // left unresolved.
        assert_eq!(select(input, &["build[1.2]"]).unwrap(), vec!["build[1.2]"]);
    }

    fn go_registry() -> ProviderRegistry {
        let mut reg = test_registry();
        reg.register(Box::new(crate::providers::go::GoProvider::new(Arc::new(
            FileTracker::new(),
        ))));
        reg
    }

    fn input_error(input: &str, registry: &ProviderRegistry, roots: &[crate::import::ImportRoot]) -> String {
        let module = parser::parse(input, "<test>").unwrap();
        match load(&module, &Map::new(), registry, &EmptyStore, roots) {
            Err(error @ LoadError::InvalidInputs { .. }) => error.to_string(),
            Err(error) => panic!("expected invalid inputs, got {error}"),
            Ok(_) => panic!("expected invalid inputs"),
        }
    }

    #[test]
    fn unknown_field_on_matrix_target_is_rejected() {
        let input = r#"
let module = [".", "open"]

go-test[module] = go.test {
  module = module
  flags = ["-short"]
  package = "./..."
}
"#;
        assert_eq!(
            input_error(input, &go_registry(), &[]),
            "<test>:4:1: target 'go-test[\".\"]': unknown field 'module'; valid fields: package, flags, inputs, \
             report, goos, goarch, cgo, depends_on, after, concurrency, uncached, dir, env"
        );
    }

    #[test]
    fn env_values_must_be_strings() {
        let error = input_error(
            "build = exec { command = \"true\" env = { A = 1 } }\n",
            &test_registry(),
            &[],
        );
        assert!(error.contains("env"), "{error}");
    }

    #[test]
    fn unknown_field_suggests_closest_name() {
        let error = input_error("build = exec { comand = \"true\" }\n", &test_registry(), &[]);
        assert!(
            error.starts_with("<test>:1:1: target 'build': unknown field 'comand' (did you mean 'command'?)"),
            "{error}"
        );
        let error = input_error(
            "build = exec { command = \"true\" dri = \"x\" }\n",
            &test_registry(),
            &[],
        );
        assert!(error.contains("(did you mean 'dir'?)"), "{error}");
    }

    #[test]
    fn field_types_are_checked_against_the_schema() {
        let cases = [
            (
                "build = go.test { package = \"./...\" flags = \"-short\" }",
                "field 'flags': expected [string], got \"-short\"",
            ),
            (
                "build = go.test { package = \"./...\" report = \"test\" }",
                "field 'report': expected \"suites\" | \"tests\" | \"verbose\", got \"test\"",
            ),
            ("build = go.test { flags = [] }", "missing required field 'package'"),
            (
                "let flags = [1]\nbuild = go.test { package = \"./...\" flags = flags }",
                "field 'flags[0]': expected string, got number",
            ),
            (
                "build = go.test { package = \"./...\" concurrency = \"1\" }",
                "field 'concurrency': expected number, got \"1\"",
            ),
            (
                "build = go.test { package = \"./...\" uncached = [true] }",
                "field 'uncached[0]': expected string, got bool",
            ),
        ];
        for (input, expected) in cases {
            let error = input_error(input, &go_registry(), &[]);
            assert!(error.ends_with(expected), "{input}: {error}");
        }
    }

    #[test]
    fn upstream_outputs_are_checked_against_their_declared_type() {
        let input = r#"
pass = go.test { package = "./..." }
build = go.test { package = "./..." dir = pass.passed }
"#;
        let error = input_error(input, &go_registry(), &[]);
        assert!(error.ends_with("field 'dir': expected string, got bool"), "{error}");

        // exec's values command declares no types, so its outputs pass
        // until the target runs.
        let input = r#"
version = exec { command = "true" values = "echo {}" }
build = go.test { package = version.package }
"#;
        let module = parser::parse(input, "<test>").unwrap();
        load(&module, &Map::new(), &go_registry(), &EmptyStore, &[]).unwrap();
    }

    #[test]
    fn engine_fields_are_accepted_on_every_resource() {
        let input = r#"
setup = exec { command = "true" }
build = exec {
  command = "true"
  outputs = "out"
  depends_on = [setup]
  after = [setup]
  concurrency = 1
  uncached = ["out"]
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        load(&module, &Map::new(), &test_registry(), &EmptyStore, &[]).unwrap();
    }

    #[test]
    fn flattened_input_fields_are_accepted() {
        let input = r#"
app = go.exe {
  package = "./cmd/app"
  goos = "linux"
  goarch = "arm64"
  cgo = false
}
"#;
        let module = parser::parse(input, "<test>").unwrap();
        load(&module, &Map::new(), &go_registry(), &EmptyStore, &[]).unwrap();
    }

    #[test]
    fn defaulted_fields_may_be_omitted() {
        let module = parser::parse("lint = go.lint {}\n", "<test>").unwrap();
        load(&module, &Map::new(), &go_registry(), &EmptyStore, &[]).unwrap();
    }

    #[test]
    fn parameterized_target_inputs_are_checked_when_invoked() {
        let input = r#"
image(tag : string) = exec {
  command = "build #{tag}"
  tags = [tag]
}
group publish(tag : string) = [image(tag = tag)]
"#;
        let module = parser::parse(input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "publish".into(),
            args: vec![("tag".into(), "v1".into())],
        }];
        let error = match load_selected(&module, &Map::new(), &test_registry(), &EmptyStore, &[], &invocations) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected invalid inputs"),
        };
        assert!(
            error.starts_with("<test>:2:1: target 'image(tag=\"v1\")': unknown field 'tags'"),
            "{error}"
        );
    }

    #[test]
    fn module_targets_are_checked_and_module_params_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        write_module(
            dir.path(),
            "mymod",
            "mymod",
            "param name : string\nbuild = exec { command = \"build #{name}\" outptu = \"out\" }\n",
        );
        let error = input_error(
            "inst = mymod { name = \"app\" }\n",
            &test_registry(),
            &roots(dir.path(), "mymod"),
        );
        assert!(error.contains("target 'inst.build': unknown field 'outptu'"), "{error}");

        write_module(
            dir.path(),
            "mymod",
            "mymod",
            "param name : string\nbuild = exec { command = \"build #{name}\" outputs = \"out\" }\noutput path = build.path\n",
        );
        let module = parser::parse("inst = mymod { name = \"app\" }\n", "<test>").unwrap();
        load(
            &module,
            &Map::new(),
            &test_registry(),
            &EmptyStore,
            &roots(dir.path(), "mymod"),
        )
        .unwrap();
    }
}
