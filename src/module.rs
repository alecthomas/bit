use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::ast::{Expr, Field, Module, Phase, Statement, StringPart, Target};
use crate::dag::{AFTER_FIELD, DEPENDS_ON_FIELD, Dag, DagNode, collect_dependency_refs, collect_target_refs};
use crate::expr::{self, Scope};
use crate::loader::LoadError;
use crate::output::TargetWriter;
use crate::provider::{
    ApplyResult as ProviderApplyResult, BoxError, CachePolicy, Changes, DynResource, PlanAction, PlanResult,
    ProviderRegistry, ResourceKind, ResourceSchema, StructField, StructType, Target as ProviderTarget,
};
use crate::state::StateStore;
use crate::value::{Map, Value, validate_type};

/// Provider name of module instance nodes.
const INSTANCE_PROVIDER: &str = "module";

/// Modifiers written on a module instance. They describe the instance as a
/// whole, so every target it expands to inherits them.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstanceModifiers {
    pub phase: Phase,
    pub protected: bool,
    pub explicit: bool,
    pub large: bool,
}

impl InstanceModifiers {
    pub fn of(target: &Target) -> Self {
        Self {
            phase: target.phase,
            protected: target.protected,
            explicit: target.explicit,
            large: target.large,
        }
    }

    /// Combine with an inner target's own modifiers. A target keeps its own
    /// phase inside a default-phase instance and takes the instance's phase
    /// otherwise; a different explicit phase would contradict the instance.
    fn merge(self, instance: &str, target: &Target) -> Result<Self, LoadError> {
        let phase = match (self.phase, target.phase) {
            (Phase::Default, phase) => phase,
            (phase, Phase::Default) => phase,
            (instance_phase, target_phase) if instance_phase == target_phase => instance_phase,
            (instance_phase, target_phase) => {
                return Err(LoadError::PhaseConflict {
                    pos: target.pos.clone(),
                    target: target.name.clone(),
                    target_phase,
                    instance: instance.to_owned(),
                    instance_phase,
                });
            }
        };
        Ok(Self {
            phase,
            protected: self.protected || target.protected,
            explicit: self.explicit || target.explicit,
            large: self.large || target.large,
        })
    }

    fn apply_to(self, target: &mut Target) {
        target.phase = self.phase;
        target.protected = self.protected;
        target.explicit = self.explicit;
        target.large = self.large;
    }
}

/// Whether `node` is a module instance. Its fields are the module's output
/// expressions, not inputs of a resource.
pub fn is_instance(node: &DagNode) -> bool {
    node.provider == INSTANCE_PROVIDER
}

/// A pass-through resource for module instance targets.
///
/// The module instance node sits atop a module's inner targets in the DAG.
/// Its fields are the module's declared output expressions (rewritten to
/// reference namespaced inner targets). The engine evaluates those fields
/// and this resource passes them through as outputs, making them available
/// to downstream targets as `instance.output_name`.
pub struct ModuleResource {
    pub resource_schema: ResourceSchema,
}

impl DynResource for ModuleResource {
    fn name(&self) -> &str {
        INSTANCE_PROVIDER
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn schema(&self) -> ResourceSchema {
        self.resource_schema.clone()
    }

    fn resolve(&self, _inputs: &Map) -> Result<std::collections::BTreeMap<String, crate::sha256::SHA256>, BoxError> {
        Ok(std::collections::BTreeMap::new())
    }

    fn plan(&self, _inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError> {
        let action = if prior_state.is_some() {
            PlanAction::None
        } else {
            PlanAction::Create
        };
        Ok(PlanResult {
            action,
            description: "module outputs".into(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Map,
        _prior_state: Option<&serde_json::Value>,
        _changes: &Changes,
        _writer: &TargetWriter,
    ) -> Result<ProviderApplyResult<serde_json::Value, Map>, BoxError> {
        Ok(ProviderApplyResult {
            outputs: inputs.clone(),
            state: Some(serde_json::json!({})),
        })
    }

    fn destroy(
        &self,
        _prior_state: &ProviderTarget<serde_json::Value>,
        _writer: &TargetWriter,
    ) -> Result<(), BoxError> {
        Ok(())
    }

    /// A module instance only forwards the outputs of its inner targets, which
    /// carry their own policies. There is no result of its own to share.
    fn cache_policy(&self, _inputs: &Map) -> CachePolicy {
        CachePolicy::Local
    }
}

/// Resolve a module file path from provider/resource names.
///
/// With one provider per import root, lookup is by exact match on the
/// provider name; the resource is `<root.path>/<resource>.bit`. Provider
/// uniqueness is enforced at import resolution time (`ImportError::ProviderConflict`).
pub fn resolve_module_path(
    import_roots: &[crate::import::ImportRoot],
    provider: &str,
    resource: &str,
) -> Option<std::path::PathBuf> {
    let root = import_roots.iter().find(|r| r.provider == provider)?;
    let path = root.path.join(format!("{resource}.bit"));
    if path.exists() { Some(path) } else { None }
}

/// Parsed interface of a module file.
struct ModuleInterface {
    params: Vec<crate::ast::Param>,
    lets: Vec<crate::ast::Let>,
    targets: Vec<Target>,
    outputs: Vec<crate::ast::Output>,
    groups: Vec<crate::ast::Group>,
}

fn parse_module_interface(module: &Module) -> ModuleInterface {
    let mut iface = ModuleInterface {
        params: Vec::new(),
        lets: Vec::new(),
        targets: Vec::new(),
        outputs: Vec::new(),
        groups: Vec::new(),
    };
    for stmt in &module.statements {
        match stmt {
            Statement::Import(_) => {}
            Statement::Param(p) => iface.params.push(p.clone()),
            Statement::Let(l) => iface.lets.push(l.clone()),
            Statement::Target(b) => iface.targets.push(b.clone()),
            Statement::Output(o) => iface.outputs.push(o.clone()),
            Statement::Group(t) => iface.groups.push(t.clone()),
        }
    }
    iface
}

/// Context for expanding a module target into the DAG.
pub struct ExpandContext<'a> {
    pub scope: &'a mut Scope,
    pub registry: &'a ProviderRegistry,
    pub store: &'a dyn StateStore,
    pub dag: &'a mut Dag,
    pub import_roots: &'a [crate::import::ImportRoot],
}

/// Expand a module target into namespaced inner targets in the DAG.
///
/// Given `instance_name = provider.resource { fields... }` where the provider
/// resolves to a `.bit` module file, this function:
///
/// 1. Parses the module file
/// 2. Maps outer fields to module params
/// 3. Creates namespaced inner targets (`instance.inner_target`)
/// 4. Creates a synthetic module instance node for output forwarding
/// 5. Wires all dependency edges
///
/// Every target the instance expands to inherits `modifiers`.
pub fn expand_module(
    instance_name: &str,
    module_path: &Path,
    outer_fields: &[Field],
    modifiers: InstanceModifiers,
    ctx: &mut ExpandContext<'_>,
) -> Result<(), LoadError> {
    let source = std::fs::read_to_string(module_path)
        .map_err(|e| LoadError::ModuleLoad(module_path.display().to_string(), e.to_string()))?;
    let module_ast = crate::parser::parse(&source, &module_path.display().to_string())
        .map_err(|e| LoadError::ModuleParse(module_path.display().to_string(), e.message))?;
    crate::loader::validate_names(&module_ast)?;

    let iface = parse_module_interface(&module_ast);
    let inner_target_names: HashSet<String> = iface.targets.iter().map(|b| b.name.clone()).collect();
    let inner_group_names: HashSet<String> = iface.groups.iter().map(|group| group.name.clone()).collect();
    let param_names: HashSet<String> = iface.params.iter().map(|p| p.name.clone()).collect();

    // Map outer fields to param expressions
    let mut param_exprs: HashMap<String, Expr> = HashMap::new();
    for field in outer_fields {
        if (field.name == DEPENDS_ON_FIELD || field.name == AFTER_FIELD) && !param_names.contains(&field.name) {
            continue;
        }
        param_exprs.insert(field.name.clone(), field.value.clone());
    }

    // Build unified substitution map: every param and let becomes a substitution
    // so inner target fields are self-contained.
    let mut substitutions: HashMap<String, Expr> = HashMap::new();
    let mut eval_scope = ctx.scope.clone();

    // Process params in declaration order
    for param in &iface.params {
        let value_expr = if let Some(expr) = param_exprs.get(&param.name) {
            expr.clone()
        } else if let Some(default) = &param.default {
            default.clone()
        } else {
            return Err(LoadError::MissingParam {
                pos: param.pos.clone(),
                name: param.name.clone(),
            });
        };

        match expr::eval(&value_expr, ctx.scope) {
            Ok(value) => {
                validate_type(&value, &param.typ).map_err(|message| LoadError::TypeError {
                    pos: param.pos.clone(),
                    name: param.name.clone(),
                    message,
                })?;
                eval_scope.set(&param.name, value.clone());
                substitutions.insert(param.name.clone(), value.to_expr());
            }
            Err(_) => {
                // Deferred: references outer target outputs not yet available.
                // Substitute the outer expression directly.
                substitutions.insert(param.name.clone(), value_expr);
            }
        }
    }

    // Process let bindings in declaration order
    for let_binding in &iface.lets {
        let rewritten = rewrite_expr(&let_binding.value, &inner_target_names, &substitutions, instance_name);
        match expr::eval(&rewritten, &eval_scope) {
            Ok(value) => {
                if let Some(typ) = &let_binding.typ {
                    validate_type(&value, typ).map_err(|message| LoadError::TypeError {
                        pos: let_binding.pos.clone(),
                        name: let_binding.name.clone(),
                        message,
                    })?;
                }
                eval_scope.set(&let_binding.name, value.clone());
                substitutions.insert(let_binding.name.clone(), value.to_expr());
            }
            Err(_) => {
                substitutions.insert(let_binding.name.clone(), rewritten);
            }
        }
    }

    // Create inner target DagNodes
    for target in &iface.targets {
        let qualified_name = format!("{instance_name}.{}", target.name);
        let target_modifiers = modifiers.merge(instance_name, target)?;

        if !target.params.is_empty() {
            let mut declaration = target.clone();
            declaration.name = qualified_name;
            target_modifiers.apply_to(&mut declaration);
            declaration.fields = target
                .fields
                .iter()
                .map(|field| Field {
                    name: field.name.clone(),
                    value: rewrite_expr(&field.value, &inner_target_names, &substitutions, instance_name),
                })
                .collect();
            for param in &mut declaration.params {
                param.default = param
                    .default
                    .as_ref()
                    .map(|value| rewrite_expr(value, &inner_target_names, &substitutions, instance_name));
            }
            ctx.dag.add_target_declaration(declaration);
            continue;
        }

        // Check for nested module
        if let Some(nested_path) = resolve_module_path(ctx.import_roots, &target.provider, &target.resource) {
            let rewritten_fields: Vec<Field> = target
                .fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    value: rewrite_expr(&f.value, &inner_target_names, &substitutions, instance_name),
                })
                .collect();
            expand_module(&qualified_name, &nested_path, &rewritten_fields, target_modifiers, ctx)?;
            continue;
        }

        let resource = ctx
            .registry
            .get_resource(&target.provider, &target.resource)
            .ok_or_else(|| LoadError::UnknownResource {
                pos: target.pos.clone(),
                provider: target.provider.clone(),
                resource: target.resource.clone(),
            })?;

        let rewritten_fields: Vec<Field> = target
            .fields
            .iter()
            .map(|f| Field {
                name: f.name.clone(),
                value: rewrite_expr(&f.value, &inner_target_names, &substitutions, instance_name),
            })
            .collect();

        let prior_state = ctx.store.load(&qualified_name)?;

        ctx.dag.add_node(DagNode {
            pos: target.pos.clone(),
            name: qualified_name.clone(),
            doc: target.doc.clone(),
            phase: target_modifiers.phase,
            provider: target.provider.clone(),
            resource_name: target.resource.clone(),
            protected: target_modifiers.protected,
            explicit: target_modifiers.explicit,
            large: target_modifiers.large,
            concurrency_group: qualified_name.clone(),
            fields: rewritten_fields,
            resource,
            prior_state,
        })?;

        ctx.scope
            .define(
                &qualified_name,
                crate::expr::SymbolKind::Target,
                Value::strct(Map::new()),
            )
            .map_err(|existing| LoadError::DuplicateName {
                pos: target.pos.clone(),
                name: qualified_name.clone(),
                existing: existing.as_str(),
            })?;
    }

    // Wire internal dependency edges for inner targets
    for target in &iface.targets {
        let qualified_name = format!("{instance_name}.{}", target.name);
        if !ctx.dag.has_target(&qualified_name) {
            continue; // Was a nested module, edges handled by recursive expand
        }

        let node_fields = &ctx.dag.get_node(&qualified_name).expect("just added").fields.clone();

        for dep in collect_target_refs(node_fields, ctx.scope).map_err(|source| LoadError::Eval {
            pos: target.pos.clone(),
            source,
        })? {
            if ctx.dag.has_target(&dep) && dep != qualified_name {
                ctx.dag.add_dep_edge(&dep, &qualified_name)?;
            }
        }
        for dep in
            collect_dependency_refs(node_fields, DEPENDS_ON_FIELD, ctx.scope).map_err(|source| LoadError::Eval {
                pos: target.pos.clone(),
                source,
            })?
        {
            if ctx.dag.has_target(&dep) && dep != qualified_name {
                ctx.dag.add_dep_edge(&dep, &qualified_name)?;
            }
        }
        for dep in collect_dependency_refs(node_fields, AFTER_FIELD, ctx.scope).map_err(|source| LoadError::Eval {
            pos: target.pos.clone(),
            source,
        })? {
            if ctx.dag.has_target(&dep) && dep != qualified_name {
                ctx.dag.add_ordering_edge(&dep, &qualified_name)?;
            }
        }
    }

    // Handle non-param depends_on/after: create edges from external targets
    // to all inner targets so the module waits for them.
    for field in outer_fields {
        if field.name == DEPENDS_ON_FIELD && !param_names.contains(DEPENDS_ON_FIELD) {
            for dep in
                collect_dependency_refs(std::slice::from_ref(field), DEPENDS_ON_FIELD, ctx.scope).map_err(|source| {
                    LoadError::Eval {
                        pos: crate::ast::Pos::default(),
                        source,
                    }
                })?
            {
                if ctx.dag.has_target(&dep) {
                    for inner in &iface.targets {
                        let qname = format!("{instance_name}.{}", inner.name);
                        if ctx.dag.has_target(&qname) {
                            ctx.dag.add_dep_edge(&dep, &qname)?;
                        }
                    }
                }
            }
        }
        if field.name == AFTER_FIELD && !param_names.contains(AFTER_FIELD) {
            for dep in
                collect_dependency_refs(std::slice::from_ref(field), AFTER_FIELD, ctx.scope).map_err(|source| {
                    LoadError::Eval {
                        pos: crate::ast::Pos::default(),
                        source,
                    }
                })?
            {
                if ctx.dag.has_target(&dep) {
                    for inner in &iface.targets {
                        let qname = format!("{instance_name}.{}", inner.name);
                        if ctx.dag.has_target(&qname) {
                            ctx.dag.add_ordering_edge(&dep, &qname)?;
                        }
                    }
                }
            }
        }
    }

    // Create module instance node for output forwarding.
    let output_fields: Vec<Field> = iface
        .outputs
        .iter()
        .map(|o| Field {
            name: o.name.clone(),
            value: rewrite_expr(&o.value, &inner_target_names, &substitutions, instance_name),
        })
        .collect();

    // Build schema from the module's declared params and outputs
    let schema_inputs: Vec<(String, StructField)> = iface
        .params
        .iter()
        .map(|p| {
            let typ = if p.default.is_some() {
                crate::value::Type::Optional(Box::new(p.typ.clone()))
            } else {
                p.typ.clone()
            };
            (
                p.name.clone(),
                StructField {
                    typ,
                    default: p.default.as_ref().and_then(|d| expr::eval(d, &eval_scope).ok()),
                    description: p.doc.clone(),
                },
            )
        })
        .collect();
    let schema_outputs: Vec<(String, StructField)> = iface
        .outputs
        .iter()
        .map(|o| {
            (
                o.name.clone(),
                StructField {
                    typ: crate::value::Type::String,
                    default: None,
                    description: o.doc.clone(),
                },
            )
        })
        .collect();
    let resource_schema = ResourceSchema {
        kind: ResourceKind::Build,
        inputs: StructType {
            description: module_ast
                .doc
                .clone()
                .or_else(|| Some(format!("Module from {}", module_path.display()))),
            fields: schema_inputs,
        },
        outputs: StructType {
            description: None,
            fields: schema_outputs,
        },
    };

    let prior_state = ctx.store.load(instance_name)?;
    ctx.dag.add_node(DagNode {
        pos: crate::ast::Pos::default(),
        name: instance_name.to_owned(),
        doc: None,
        phase: modifiers.phase,
        provider: INSTANCE_PROVIDER.into(),
        resource_name: INSTANCE_PROVIDER.into(),
        protected: modifiers.protected,
        explicit: modifiers.explicit,
        large: modifiers.large,
        concurrency_group: instance_name.to_owned(),
        fields: output_fields,
        resource: Box::new(ModuleResource { resource_schema }),
        prior_state,
    })?;

    // Module instance depends on all inner targets (so outputs can reference them)
    for target in &iface.targets {
        let qualified_name = format!("{instance_name}.{}", target.name);
        if ctx.dag.has_target(&qualified_name) {
            ctx.dag.add_dep_edge(&qualified_name, instance_name)?;
        }
    }

    // Register module groups as declarations. The invocation pass resolves
    // their arguments and materializes any parameterized target calls.
    for group in &iface.groups {
        let mut declaration = group.clone();
        declaration.name = format!("{instance_name}.{}", group.name);
        for param in &mut declaration.params {
            param.default = param
                .default
                .as_ref()
                .map(|value| rewrite_expr(value, &inner_target_names, &substitutions, instance_name));
        }
        for call in &mut declaration.members {
            let root_name = call.name.split('.').next().unwrap_or(&call.name);
            if inner_target_names.contains(root_name) || inner_group_names.contains(root_name) {
                call.name = format!("{instance_name}.{}", call.name);
            }
            if let Some(keys) = &mut call.keys {
                for key in keys {
                    *key = rewrite_expr(key, &inner_target_names, &substitutions, instance_name);
                }
            }
            for arg in &mut call.args {
                arg.value = rewrite_expr(&arg.value, &inner_target_names, &substitutions, instance_name);
            }
        }
        ctx.dag.add_group_declaration(declaration);
    }

    ctx.scope
        .define(instance_name, crate::expr::SymbolKind::Target, Value::strct(Map::new()))
        .map_err(|existing| LoadError::DuplicateName {
            pos: crate::ast::Pos {
                file: module_path.to_string_lossy().into_owned(),
                line: 0,
                col: 0,
            },
            name: instance_name.to_owned(),
            existing: existing.as_str(),
        })?;

    Ok(())
}

/// Rewrite an expression for use inside an expanded module.
///
/// - Refs to inner targets are prefixed: `image.ref` -> `staging.image.ref`
///   (using a dotted first-part so scope lookup finds the namespaced key)
/// - Refs to params/lets are substituted with their value expressions
/// - All other refs pass through unchanged (outer scope)
pub(crate) fn rewrite_expr(
    expr: &Expr,
    inner_targets: &HashSet<String>,
    substitutions: &HashMap<String, Expr>,
    prefix: &str,
) -> Expr {
    match expr {
        Expr::Ref(parts) => {
            let first = &parts[0];
            if inner_targets.contains(first) {
                let mut new_parts = parts.clone();
                new_parts[0] = format!("{prefix}.{first}");
                return Expr::Ref(new_parts);
            }
            if parts.len() == 1
                && let Some(sub) = substitutions.get(first)
            {
                return sub.clone();
            }
            expr.clone()
        }
        Expr::TargetRef(name) => Expr::TargetRef(if inner_targets.contains(name) {
            format!("{prefix}.{name}")
        } else {
            name.clone()
        }),
        Expr::MatrixRef { name, keys, fields } => Expr::MatrixRef {
            name: if inner_targets.contains(name) {
                format!("{prefix}.{name}")
            } else {
                name.clone()
            },
            keys: keys
                .iter()
                .map(|key| rewrite_expr(key, inner_targets, substitutions, prefix))
                .collect(),
            fields: fields.clone(),
        },
        Expr::TargetCall {
            name,
            keys,
            args,
            fields,
        } => Expr::TargetCall {
            name: if inner_targets.contains(name) {
                format!("{prefix}.{name}")
            } else {
                name.clone()
            },
            keys: keys.as_ref().map(|keys| {
                keys.iter()
                    .map(|key| rewrite_expr(key, inner_targets, substitutions, prefix))
                    .collect()
            }),
            args: args
                .iter()
                .map(|arg| Field {
                    name: arg.name.clone(),
                    value: rewrite_expr(&arg.value, inner_targets, substitutions, prefix),
                })
                .collect(),
            fields: fields.clone(),
        },
        Expr::Str(parts) => {
            let new_parts: Vec<StringPart> = parts
                .iter()
                .map(|p| match p {
                    StringPart::Literal(_) => p.clone(),
                    StringPart::Interpolation(e) => {
                        StringPart::Interpolation(rewrite_expr(e, inner_targets, substitutions, prefix))
                    }
                })
                .collect();
            Expr::Str(new_parts)
        }
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|e| rewrite_expr(e, inner_targets, substitutions, prefix))
                .collect(),
        ),
        Expr::Map(fields) => Expr::Map(
            fields
                .iter()
                .map(|f| crate::ast::MapEntry {
                    name: f.name.clone(),
                    typ: f.typ.clone(),
                    value: rewrite_expr(&f.value, inner_targets, substitutions, prefix),
                })
                .collect(),
        ),
        Expr::Call(name, args) => Expr::Call(
            name.clone(),
            args.iter()
                .map(|e| rewrite_expr(e, inner_targets, substitutions, prefix))
                .collect(),
        ),
        Expr::Pipe(inner, name, args) => Expr::Pipe(
            Box::new(rewrite_expr(inner, inner_targets, substitutions, prefix)),
            name.clone(),
            args.iter()
                .map(|e| rewrite_expr(e, inner_targets, substitutions, prefix))
                .collect(),
        ),
        Expr::If(cond, then_val, else_val) => Expr::If(
            Box::new(rewrite_expr(cond, inner_targets, substitutions, prefix)),
            Box::new(rewrite_expr(then_val, inner_targets, substitutions, prefix)),
            Box::new(rewrite_expr(else_val, inner_targets, substitutions, prefix)),
        ),
        Expr::BinOp(lhs, op, rhs) => Expr::BinOp(
            Box::new(rewrite_expr(lhs, inner_targets, substitutions, prefix)),
            op.clone(),
            Box::new(rewrite_expr(rhs, inner_targets, substitutions, prefix)),
        ),
        Expr::Add(lhs, rhs) => Expr::Add(
            Box::new(rewrite_expr(lhs, inner_targets, substitutions, prefix)),
            Box::new(rewrite_expr(rhs, inner_targets, substitutions, prefix)),
        ),
        Expr::Number(_) | Expr::Bool(_) | Expr::Duration(_) | Expr::Null => expr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_namespaces_inner_target_refs() {
        let inner = HashSet::from(["image".to_owned(), "app".to_owned()]);
        let subs = HashMap::new();

        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        let result = rewrite_expr(&expr, &inner, &subs, "staging");
        assert_eq!(result, Expr::Ref(vec!["staging.image".into(), "ref".into()]));
    }

    #[test]
    fn rewrite_substitutes_params() {
        let inner = HashSet::new();
        let subs = HashMap::from([("env".to_owned(), Expr::Str(vec![StringPart::Literal("prod".into())]))]);

        let expr = Expr::Ref(vec!["env".into()]);
        let result = rewrite_expr(&expr, &inner, &subs, "staging");
        assert_eq!(result, Expr::Str(vec![StringPart::Literal("prod".into())]));
    }

    #[test]
    fn rewrite_leaves_outer_refs() {
        let inner = HashSet::from(["app".to_owned()]);
        let subs = HashMap::new();

        let expr = Expr::Ref(vec!["external".into(), "path".into()]);
        let result = rewrite_expr(&expr, &inner, &subs, "staging");
        assert_eq!(result, Expr::Ref(vec!["external".into(), "path".into()]));
    }

    #[test]
    fn rewrite_handles_interpolation() {
        let inner = HashSet::from(["app".to_owned()]);
        let subs = HashMap::from([(
            "registry".to_owned(),
            Expr::Str(vec![StringPart::Literal("gcr.io".into())]),
        )]);

        let expr = Expr::Str(vec![
            StringPart::Interpolation(Expr::Ref(vec!["registry".into()])),
            StringPart::Literal("/".into()),
            StringPart::Interpolation(Expr::Ref(vec!["app".into(), "name".into()])),
        ]);
        let result = rewrite_expr(&expr, &inner, &subs, "staging");
        assert_eq!(
            result,
            Expr::Str(vec![
                StringPart::Interpolation(Expr::Str(vec![StringPart::Literal("gcr.io".into())])),
                StringPart::Literal("/".into()),
                StringPart::Interpolation(Expr::Ref(vec!["staging.app".into(), "name".into(),])),
            ])
        );
    }

    #[test]
    fn resolve_from_import_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("res.bit"), "").unwrap();
        let roots = vec![crate::import::ImportRoot {
            provider: "prov".into(),
            path: dir.path().to_path_buf(),
        }];
        let result = resolve_module_path(&roots, "prov", "res");
        assert_eq!(result, Some(dir.path().join("res.bit")));
    }

    #[test]
    fn resolve_distinct_providers() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.bit"), "").unwrap();
        std::fs::write(b.path().join("y.bit"), "").unwrap();
        let roots = vec![
            crate::import::ImportRoot {
                provider: "alpha".into(),
                path: a.path().to_path_buf(),
            },
            crate::import::ImportRoot {
                provider: "beta".into(),
                path: b.path().to_path_buf(),
            },
        ];
        assert_eq!(resolve_module_path(&roots, "alpha", "x"), Some(a.path().join("x.bit")));
        assert_eq!(resolve_module_path(&roots, "beta", "y"), Some(b.path().join("y.bit")));
        // Wrong provider name yields nothing even if a file by that name exists.
        assert_eq!(resolve_module_path(&roots, "alpha", "y"), None);
    }

    #[test]
    fn resolve_no_match_returns_none() {
        let result = resolve_module_path(&[], "prov", "res");
        assert_eq!(result, None);
    }

    #[test]
    fn value_to_expr_roundtrip() {
        let val = Value::Str("hello".into());
        assert_eq!(val.to_expr(), Expr::Str(vec![StringPart::Literal("hello".into())]));

        let val = Value::Number(42.into());
        assert_eq!(val.to_expr(), Expr::Number(42.into()));

        let val = Value::Bool(true);
        assert_eq!(val.to_expr(), Expr::Bool(true));

        let val = Value::list(vec![Value::Str("a".into())]);
        assert_eq!(
            val.to_expr(),
            Expr::List(vec![Expr::Str(vec![StringPart::Literal("a".into())])])
        );
    }
}
