use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::ast::{Block, Expr, Field, Module, Statement, StringPart};
use crate::dag::{Dag, DagNode, collect_after, collect_block_refs, collect_depends_on};
use crate::expr::{self, Scope};
use crate::loader::LoadError;
use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult as ProviderApplyResult, BoxError, DynResource, FieldSchema, PlanAction, PlanResult, ProviderRegistry,
    ResolvedFile, ResourceKind, ResourceSchema,
};
use crate::state::StateStore;
use crate::value::{Map, Value, validate_type};

/// A pass-through resource for module instance blocks.
///
/// The module instance node sits atop a module's inner blocks in the DAG.
/// Its fields are the module's declared output expressions (rewritten to
/// reference namespaced inner blocks). The engine evaluates those fields
/// and this resource passes them through as outputs, making them available
/// to downstream blocks as `instance.output_name`.
pub struct ModuleResource {
    pub resource_schema: ResourceSchema,
}

impl DynResource for ModuleResource {
    fn name(&self) -> &str {
        "module"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn schema(&self) -> ResourceSchema {
        self.resource_schema.clone()
    }

    fn resolve(&self, _inputs: &Map) -> Result<Vec<ResolvedFile>, BoxError> {
        Ok(vec![])
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
        _writer: &BlockWriter,
    ) -> Result<ProviderApplyResult<serde_json::Value, Map>, BoxError> {
        Ok(ProviderApplyResult {
            outputs: inputs.clone(),
            state: Some(serde_json::json!({})),
        })
    }

    fn destroy(&self, _prior_state: &serde_json::Value, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(
        &self,
        _prior_state: &serde_json::Value,
    ) -> Result<ProviderApplyResult<serde_json::Value, Map>, BoxError> {
        Ok(ProviderApplyResult {
            outputs: Map::new(),
            state: Some(serde_json::json!({})),
        })
    }
}

/// Resolve a module file path from provider/resource names.
///
/// Checks `.bit/modules/{provider}/{resource}.bit` under the given root.
/// Returns `None` if no matching module file exists.
pub fn resolve_module_path(root: &Path, provider: &str, resource: &str) -> Option<std::path::PathBuf> {
    let path = root.join(".bit/modules").join(provider).join(format!("{resource}.bit"));
    if path.exists() {
        return Some(path);
    }
    None
}

/// Parsed interface of a module file.
struct ModuleInterface {
    params: Vec<crate::ast::Param>,
    lets: Vec<crate::ast::Let>,
    blocks: Vec<Block>,
    outputs: Vec<crate::ast::Output>,
    targets: Vec<crate::ast::Target>,
}

fn parse_module_interface(module: &Module) -> ModuleInterface {
    let mut iface = ModuleInterface {
        params: Vec::new(),
        lets: Vec::new(),
        blocks: Vec::new(),
        outputs: Vec::new(),
        targets: Vec::new(),
    };
    for stmt in &module.statements {
        match stmt {
            Statement::Param(p) => iface.params.push(p.clone()),
            Statement::Let(l) => iface.lets.push(l.clone()),
            Statement::Block(b) => iface.blocks.push(b.clone()),
            Statement::Output(o) => iface.outputs.push(o.clone()),
            Statement::Target(t) => iface.targets.push(t.clone()),
        }
    }
    iface
}

/// Context for expanding a module block into the DAG.
pub struct ExpandContext<'a> {
    pub scope: &'a mut Scope,
    pub registry: &'a ProviderRegistry,
    pub store: &'a dyn StateStore,
    pub dag: &'a mut Dag,
    pub root: &'a Path,
}

/// Expand a module block into namespaced inner blocks in the DAG.
///
/// Given `instance_name = provider.resource { fields... }` where the provider
/// resolves to a `.bit` module file, this function:
///
/// 1. Parses the module file
/// 2. Maps outer fields to module params
/// 3. Creates namespaced inner blocks (`instance.inner_block`)
/// 4. Creates a synthetic module instance node for output forwarding
/// 5. Wires all dependency edges
pub fn expand_module(
    instance_name: &str,
    module_path: &Path,
    outer_fields: &[Field],
    ctx: &mut ExpandContext<'_>,
) -> Result<(), LoadError> {
    let source = std::fs::read_to_string(module_path)
        .map_err(|e| LoadError::ModuleLoad(module_path.display().to_string(), e.to_string()))?;
    let module_ast = crate::parser::parse(&source, &module_path.display().to_string())
        .map_err(|e| LoadError::ModuleParse(module_path.display().to_string(), e.message))?;

    let iface = parse_module_interface(&module_ast);
    let inner_block_names: HashSet<String> = iface.blocks.iter().map(|b| b.name.clone()).collect();
    let param_names: HashSet<String> = iface.params.iter().map(|p| p.name.clone()).collect();

    // Map outer fields to param expressions
    let mut param_exprs: HashMap<String, Expr> = HashMap::new();
    for field in outer_fields {
        if (field.name == "depends_on" || field.name == "after") && !param_names.contains(&field.name) {
            continue;
        }
        param_exprs.insert(field.name.clone(), field.value.clone());
    }

    // Build unified substitution map: every param and let becomes a substitution
    // so inner block fields are self-contained.
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
                // Deferred: references outer block outputs not yet available.
                // Substitute the outer expression directly.
                substitutions.insert(param.name.clone(), value_expr);
            }
        }
    }

    // Process let bindings in declaration order
    for let_binding in &iface.lets {
        let rewritten = rewrite_expr(&let_binding.value, &inner_block_names, &substitutions, instance_name);
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

    // Create inner block DagNodes
    for block in &iface.blocks {
        let qualified_name = format!("{instance_name}.{}", block.name);

        // Check for nested module
        if let Some(nested_path) = resolve_module_path(ctx.root, &block.provider, &block.resource) {
            let rewritten_fields: Vec<Field> = block
                .fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    value: rewrite_expr(&f.value, &inner_block_names, &substitutions, instance_name),
                })
                .collect();
            expand_module(&qualified_name, &nested_path, &rewritten_fields, ctx)?;
            continue;
        }

        let resource = ctx
            .registry
            .get_resource(&block.provider, &block.resource)
            .ok_or_else(|| LoadError::UnknownResource {
                pos: block.pos.clone(),
                provider: block.provider.clone(),
                resource: block.resource.clone(),
            })?;

        let rewritten_fields: Vec<Field> = block
            .fields
            .iter()
            .map(|f| Field {
                name: f.name.clone(),
                value: rewrite_expr(&f.value, &inner_block_names, &substitutions, instance_name),
            })
            .collect();

        let prior_state = ctx.store.load(&qualified_name)?;

        ctx.dag.add_node(DagNode {
            pos: block.pos.clone(),
            name: qualified_name.clone(),
            provider: block.provider.clone(),
            resource_name: block.resource.clone(),
            protected: block.protected,
            fields: rewritten_fields,
            resource,
            prior_state,
        })?;

        ctx.scope.set(&qualified_name, Value::Map(Map::new()));
    }

    // Wire internal dependency edges for inner blocks
    for block in &iface.blocks {
        let qualified_name = format!("{instance_name}.{}", block.name);
        if !ctx.dag.has_block(&qualified_name) {
            continue; // Was a nested module, edges handled by recursive expand
        }

        let node_fields = &ctx.dag.get_node(&qualified_name).expect("just added").fields.clone();

        for dep in collect_block_refs(node_fields) {
            if ctx.dag.has_block(&dep) && dep != qualified_name {
                ctx.dag.add_dep_edge(&dep, &qualified_name)?;
            }
        }
        for dep in collect_depends_on(node_fields) {
            if ctx.dag.has_block(&dep) && dep != qualified_name {
                ctx.dag.add_dep_edge(&dep, &qualified_name)?;
            }
        }
        for dep in collect_after(node_fields) {
            if ctx.dag.has_block(&dep) && dep != qualified_name {
                ctx.dag.add_ordering_edge(&dep, &qualified_name)?;
            }
        }
    }

    // Handle non-param depends_on/after: create edges from external blocks
    // to all inner blocks so the module waits for them.
    for field in outer_fields {
        if field.name == "depends_on"
            && !param_names.contains("depends_on")
            && let Expr::List(items) = &field.value
        {
            for item in items {
                if let Expr::Ref(parts) = item {
                    let dep = &parts[0];
                    if ctx.dag.has_block(dep) {
                        for inner in &iface.blocks {
                            let qname = format!("{instance_name}.{}", inner.name);
                            if ctx.dag.has_block(&qname) {
                                ctx.dag.add_dep_edge(dep, &qname)?;
                            }
                        }
                    }
                }
            }
        }
        if field.name == "after"
            && !param_names.contains("after")
            && let Expr::List(items) = &field.value
        {
            for item in items {
                if let Expr::Ref(parts) = item {
                    let dep = &parts[0];
                    if ctx.dag.has_block(dep) {
                        for inner in &iface.blocks {
                            let qname = format!("{instance_name}.{}", inner.name);
                            if ctx.dag.has_block(&qname) {
                                ctx.dag.add_ordering_edge(dep, &qname)?;
                            }
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
            value: rewrite_expr(&o.value, &inner_block_names, &substitutions, instance_name),
        })
        .collect();

    // Build schema from the module's declared params and outputs
    let schema_inputs: Vec<FieldSchema> = iface
        .params
        .iter()
        .map(|p| FieldSchema {
            name: p.name.clone(),
            typ: p.typ.clone(),
            required: p.default.is_none(),
            default: p.default.as_ref().and_then(|d| expr::eval(d, &eval_scope).ok()),
            description: p.doc.clone(),
        })
        .collect();
    let schema_outputs: Vec<FieldSchema> = iface
        .outputs
        .iter()
        .map(|o| FieldSchema {
            name: o.name.clone(),
            typ: crate::value::Type::String, // outputs are untyped; default to string
            required: true,
            default: None,
            description: o.doc.clone(),
        })
        .collect();
    let resource_schema = ResourceSchema {
        description: module_ast
            .doc
            .clone()
            .unwrap_or_else(|| format!("Module from {}", module_path.display())),
        kind: ResourceKind::Build,
        inputs: schema_inputs,
        outputs: schema_outputs,
    };

    let prior_state = ctx.store.load(instance_name)?;
    ctx.dag.add_node(DagNode {
        pos: crate::ast::Pos::default(),
        name: instance_name.to_owned(),
        provider: "module".into(),
        resource_name: "module".into(),
        protected: false,
        fields: output_fields,
        resource: Box::new(ModuleResource { resource_schema }),
        prior_state,
    })?;

    // Module instance depends on all inner blocks (so outputs can reference them)
    for block in &iface.blocks {
        let qualified_name = format!("{instance_name}.{}", block.name);
        if ctx.dag.has_block(&qualified_name) {
            ctx.dag.add_dep_edge(&qualified_name, instance_name)?;
        }
    }

    // Register module targets with namespaced block names
    for target in &iface.targets {
        let qualified_blocks: Vec<String> = target
            .blocks
            .iter()
            .map(|b| {
                let root_name = b.split('.').next().unwrap_or(b);
                if inner_block_names.contains(root_name) {
                    format!("{instance_name}.{b}")
                } else {
                    b.clone()
                }
            })
            .collect();
        ctx.dag.add_target(
            format!("{instance_name}.{}", target.name),
            qualified_blocks,
            target.doc.clone(),
        );
    }

    ctx.scope.set(instance_name, Value::Map(Map::new()));

    Ok(())
}

/// Rewrite an expression for use inside an expanded module.
///
/// - Refs to inner blocks are prefixed: `image.ref` -> `staging.image.ref`
///   (using a dotted first-part so scope lookup finds the namespaced key)
/// - Refs to params/lets are substituted with their value expressions
/// - All other refs pass through unchanged (outer scope)
fn rewrite_expr(
    expr: &Expr,
    inner_blocks: &HashSet<String>,
    substitutions: &HashMap<String, Expr>,
    prefix: &str,
) -> Expr {
    match expr {
        Expr::Ref(parts) => {
            let first = &parts[0];
            if inner_blocks.contains(first) {
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
        Expr::Str(parts) => {
            let new_parts: Vec<StringPart> = parts
                .iter()
                .map(|p| match p {
                    StringPart::Literal(_) => p.clone(),
                    StringPart::Interpolation(e) => {
                        StringPart::Interpolation(rewrite_expr(e, inner_blocks, substitutions, prefix))
                    }
                })
                .collect();
            Expr::Str(new_parts)
        }
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|e| rewrite_expr(e, inner_blocks, substitutions, prefix))
                .collect(),
        ),
        Expr::Map(fields) => Expr::Map(
            fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    value: rewrite_expr(&f.value, inner_blocks, substitutions, prefix),
                })
                .collect(),
        ),
        Expr::Call(name, args) => Expr::Call(
            name.clone(),
            args.iter()
                .map(|e| rewrite_expr(e, inner_blocks, substitutions, prefix))
                .collect(),
        ),
        Expr::Pipe(inner, name, args) => Expr::Pipe(
            Box::new(rewrite_expr(inner, inner_blocks, substitutions, prefix)),
            name.clone(),
            args.iter()
                .map(|e| rewrite_expr(e, inner_blocks, substitutions, prefix))
                .collect(),
        ),
        Expr::If(cond, then_val, else_val) => Expr::If(
            Box::new(rewrite_expr(cond, inner_blocks, substitutions, prefix)),
            Box::new(rewrite_expr(then_val, inner_blocks, substitutions, prefix)),
            Box::new(rewrite_expr(else_val, inner_blocks, substitutions, prefix)),
        ),
        Expr::BinOp(lhs, op, rhs) => Expr::BinOp(
            Box::new(rewrite_expr(lhs, inner_blocks, substitutions, prefix)),
            op.clone(),
            Box::new(rewrite_expr(rhs, inner_blocks, substitutions, prefix)),
        ),
        Expr::Add(lhs, rhs) => Expr::Add(
            Box::new(rewrite_expr(lhs, inner_blocks, substitutions, prefix)),
            Box::new(rewrite_expr(rhs, inner_blocks, substitutions, prefix)),
        ),
        Expr::Number(_) | Expr::Bool(_) | Expr::Null => expr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_namespaces_inner_block_refs() {
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
    fn value_to_expr_roundtrip() {
        let val = Value::Str("hello".into());
        assert_eq!(val.to_expr(), Expr::Str(vec![StringPart::Literal("hello".into())]));

        let val = Value::Number(42.into());
        assert_eq!(val.to_expr(), Expr::Number(42.into()));

        let val = Value::Bool(true);
        assert_eq!(val.to_expr(), Expr::Bool(true));

        let val = Value::List(vec![Value::Str("a".into())]);
        assert_eq!(
            val.to_expr(),
            Expr::List(vec![Expr::Str(vec![StringPart::Literal("a".into())])])
        );
    }
}
