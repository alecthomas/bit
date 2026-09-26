//! Bind target and block arguments before the graph is wired.

use std::collections::{HashMap, HashSet};

use crate::ast::{Block, Expr, Field, Module, Param, Statement, Target};
use crate::dag::{Dag, DagNode};
use crate::expr::{self, Scope, SymbolKind};
use crate::loader::LoadError;
use crate::matrix;
use crate::module;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::value::{Map, Type, Value, validate_type};

/// A CLI selector followed by its named arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub name: String,
    pub args: Vec<(String, String)>,
}

struct Bound {
    scope: Scope,
    values: Vec<Value>,
    substitutions: HashMap<String, Expr>,
}

struct Instantiator<'a> {
    blocks: HashMap<String, Block>,
    targets: HashMap<String, Target>,
    matrix_blocks: &'a mut HashMap<String, Vec<String>>,
    scope: &'a mut Scope,
    dag: &'a mut Dag,
    registry: &'a ProviderRegistry,
    store: &'a dyn StateStore,
    import_roots: &'a [crate::import::ImportRoot],
}

pub(crate) fn materialize(
    module: &Module,
    invocations: &[Invocation],
    concrete_blocks: &mut [Block],
    matrix_blocks: &mut HashMap<String, Vec<String>>,
    environment: &mut module::ExpandContext<'_>,
) -> Result<Vec<String>, LoadError> {
    let mut blocks = environment.dag.block_declarations().clone();
    blocks.extend(module.statements.iter().filter_map(|stmt| match stmt {
        Statement::Block(block) => Some((block.name.clone(), block.clone())),
        _ => None,
    }));
    let mut targets = environment.dag.target_declarations().clone();
    targets.extend(module.statements.iter().filter_map(|stmt| match stmt {
        Statement::Target(target) => Some((target.name.clone(), target.clone())),
        _ => None,
    }));
    let mut context = Instantiator {
        blocks,
        targets,
        matrix_blocks,
        scope: environment.scope,
        dag: environment.dag,
        registry: environment.registry,
        store: environment.store,
        import_roots: environment.import_roots,
    };

    // Blocks already present in the DAG include ordinary blocks, matrix
    // slices, and expanded module nodes. Resolve calls in all of them before
    // dependency discovery so the syntax works in every block field.
    let existing = context.dag.block_names();
    for name in existing {
        let fields = context.dag.get_node(&name).expect("listed block exists").fields.clone();
        let caller = context.scope.clone();
        let rewritten = context.materialize_fields(&fields, &caller, &mut Vec::new())?;
        context.dag.get_node_mut(&name).expect("listed block exists").fields = rewritten;
    }
    for block in concrete_blocks {
        if !block.matrix_keys.is_empty() {
            continue;
        }
        let caller = context.scope.clone();
        block.fields = context.materialize_fields(&block.fields, &caller, &mut Vec::new())?;
    }

    // Preserve existing validation and listing for targets with no parameters.
    let mut plain_targets: Vec<_> = context
        .targets
        .values()
        .filter(|target| target.params.is_empty())
        .map(|target| target.name.clone())
        .collect();
    plain_targets.sort();
    for name in plain_targets {
        context.instantiate_target(&name, &[], &context.scope.clone(), &mut Vec::new())?;
    }

    if invocations.is_empty()
        && let Some(default) = context.targets.get("default").cloned()
        && !default.params.is_empty()
    {
        let blocks = context.instantiate_target("default", &[], &context.scope.clone(), &mut Vec::new())?;
        context.dag.add_target("default".into(), blocks, default.doc);
    }

    let mut selected = Vec::new();
    for invocation in invocations {
        if let Some(target) = context.targets.get(&invocation.name).cloned() {
            let args = cli_args(&target.params, &invocation.args, &target.name, &target.pos)?;
            let name = context.target_name(&target, &args, &context.scope.clone())?;
            context.instantiate_target(&target.name, &args, &context.scope.clone(), &mut Vec::new())?;
            selected.push(name);
        } else if let Some(block) = context.blocks.get(&invocation.name).cloned() {
            let args = cli_args(&block.params, &invocation.args, &block.name, &block.pos)?;
            let name = context.instantiate_block(&block, &args, &context.scope.clone(), &mut Vec::new())?;
            if block.matrix_keys.is_empty() {
                selected.push(name);
            } else {
                selected.extend(context.matrix_slices(&name));
            }
        } else {
            if !invocation.args.is_empty() {
                return Err(invalid(
                    &crate::ast::Pos::default(),
                    &invocation.name,
                    "unknown target or block",
                ));
            }
            selected.push(invocation.name.clone());
        }
    }
    Ok(selected)
}

impl Instantiator<'_> {
    fn matrix_slices(&self, name: &str) -> Vec<String> {
        let Some(Value::Struct(_, slices)) = self.scope.get(name) else {
            return Vec::new();
        };
        let mut names: Vec<_> = slices.keys().map(|key| format!("{name}[{key}]")).collect();
        names.sort();
        names
    }

    fn target_name(&self, target: &Target, args: &[Field], caller: &Scope) -> Result<String, LoadError> {
        let bound = bind(&target.name, &target.pos, &target.params, args, caller)?;
        instance_name(&target.name, &target.params, &bound.values)
    }

    fn instantiate_target(
        &mut self,
        name: &str,
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<Vec<String>, LoadError> {
        let Some(target) = self.targets.get(name).cloned() else {
            if !args.is_empty() {
                return Err(invalid(
                    &crate::ast::Pos::default(),
                    name,
                    "this target does not accept arguments",
                ));
            }
            return self
                .dag
                .targets()
                .get(name)
                .map(|target| target.blocks.clone())
                .ok_or_else(|| invalid(&crate::ast::Pos::default(), name, "unknown target"));
        };
        let bound = bind(name, &target.pos, &target.params, args, caller)?;
        let instance = instance_name(name, &target.params, &bound.values)?;
        if let Some(existing) = self.dag.targets().get(&instance) {
            return Ok(existing.blocks.clone());
        }
        if stack.iter().any(|entry| entry == name) {
            return Err(invalid(&target.pos, name, "recursive target invocation"));
        }
        stack.push(name.to_owned());
        let mut blocks = Vec::new();
        for call in &target.blocks {
            if self.targets.contains_key(&call.name) || self.dag.targets().contains_key(&call.name) {
                if call.keys.is_some() {
                    return Err(invalid(&target.pos, &call.name, "a target cannot have matrix keys"));
                }
                blocks.extend(self.instantiate_target(&call.name, &call.args, &bound.scope, stack)?);
            } else if let Some(block) = self.blocks.get(&call.name).cloned() {
                if let Some(keys) = &call.keys {
                    blocks.push(self.select_matrix_slice(&block, keys, &call.args, &bound.scope, &mut Vec::new())?);
                } else {
                    let name = self.instantiate_block(&block, &call.args, &bound.scope, &mut Vec::new())?;
                    if block.matrix_keys.is_empty() {
                        blocks.push(name);
                    } else {
                        blocks.extend(self.matrix_slices(&name));
                    }
                }
            } else {
                if !call.args.is_empty() || call.keys.is_some() {
                    return Err(invalid(&target.pos, &call.name, "unknown block or target"));
                }
                blocks.push(call.name.clone());
            }
        }
        stack.pop();
        self.dag.add_target(instance, blocks.clone(), target.doc);
        Ok(blocks)
    }

    fn select_matrix_slice(
        &mut self,
        block: &Block,
        keys: &[Expr],
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<String, LoadError> {
        if block.matrix_keys.is_empty() || keys.len() != block.matrix_keys.len() {
            return Err(invalid(&block.pos, &block.name, "invalid matrix slice"));
        }
        let rewritten_keys: Vec<_> = keys
            .iter()
            .map(|key| self.materialize_expr(key, caller, stack))
            .collect::<Result<_, _>>()?;
        let rewritten_args = self.materialize_fields(args, caller, stack)?;
        let group = self.instantiate_block(block, &rewritten_args, caller, stack)?;
        let values: Vec<_> = rewritten_keys
            .iter()
            .map(|key| {
                expr::eval(key, caller).map_err(|source| LoadError::Eval {
                    pos: block.pos.clone(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?;
        let value_refs: Vec<_> = values.iter().collect();
        let slice = matrix::matrix_key(&group, &value_refs);
        if !self.dag.has_block(&slice) {
            return Err(invalid(
                &block.pos,
                &block.name,
                &format!("unknown matrix slice '{slice}'"),
            ));
        }
        Ok(slice)
    }

    fn instantiate_block(
        &mut self,
        block: &Block,
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<String, LoadError> {
        if block.params.is_empty() {
            if !args.is_empty() {
                return Err(invalid(&block.pos, &block.name, "this block does not accept arguments"));
            }
            return Ok(block.name.clone());
        }
        let bound = bind(&block.name, &block.pos, &block.params, args, caller)?;
        let name = instance_name(&block.name, &block.params, &bound.values)?;
        if self.dag.has_block(&name) || self.matrix_blocks.contains_key(&name) {
            return Ok(name);
        }
        if stack.iter().any(|entry| entry == &block.name) {
            return Err(invalid(&block.pos, &block.name, "recursive block invocation"));
        }
        stack.push(block.name.clone());
        let mut concrete = block.clone();
        concrete.name = name.clone();
        concrete.params.clear();
        concrete.explicit = block.explicit;
        let substitutions: HashMap<_, _> = bound
            .substitutions
            .iter()
            .filter(|(key, _)| !block.matrix_keys.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let substituted: Vec<Field> = block
            .fields
            .iter()
            .map(|field| Field {
                name: field.name.clone(),
                value: module::rewrite_expr(&field.value, &HashSet::new(), &substitutions, ""),
            })
            .collect();
        if !block.matrix_keys.is_empty() {
            concrete.fields = substituted;
            // Matrix keys may be bound block parameters. Expand in that scope,
            // then publish only the resulting nodes to the module scope.
            let mut expansion_scope = bound.scope.clone();
            let before: HashSet<_> = self.dag.block_names().into_iter().collect();
            matrix::expand_matrix(
                &concrete,
                &mut expansion_scope,
                self.registry,
                self.store,
                self.dag,
                self.matrix_blocks,
            )?;
            let group = expansion_scope.get(&name).cloned().ok_or_else(|| {
                invalid(
                    &block.pos,
                    &block.name,
                    "matrix expansion did not register its instance",
                )
            })?;
            self.scope
                .define(&name, SymbolKind::Block, group)
                .map_err(|existing| LoadError::DuplicateName {
                    pos: block.pos.clone(),
                    name: name.clone(),
                    existing: existing.as_str(),
                })?;
            let expanded: Vec<_> = self
                .dag
                .block_names()
                .into_iter()
                .filter(|node_name| !before.contains(node_name))
                .collect();
            for node_name in expanded {
                self.scope
                    .define(&node_name, SymbolKind::Block, Value::strct(Map::new()))
                    .map_err(|existing| LoadError::DuplicateName {
                        pos: block.pos.clone(),
                        name: node_name.clone(),
                        existing: existing.as_str(),
                    })?;
                let fields = self
                    .dag
                    .get_node(&node_name)
                    .ok_or_else(|| invalid(&block.pos, &block.name, "matrix slice was not registered"))?
                    .fields
                    .clone();
                let rewritten = self.materialize_fields(&fields, &expansion_scope, stack)?;
                self.dag
                    .get_node_mut(&node_name)
                    .ok_or_else(|| invalid(&block.pos, &block.name, "matrix slice was not registered"))?
                    .fields = rewritten;
            }
            self.matrix_blocks.insert(name.clone(), block.matrix_keys.clone());
            stack.pop();
            return Ok(name);
        }
        concrete.fields = self.materialize_fields(&substituted, &bound.scope, stack)?;

        if let Some(path) = module::resolve_module_path(self.import_roots, &block.provider, &block.resource) {
            let before: HashSet<_> = self.dag.block_names().into_iter().collect();
            let mut context = module::ExpandContext {
                scope: self.scope,
                registry: self.registry,
                store: self.store,
                dag: self.dag,
                import_roots: self.import_roots,
            };
            module::expand_module(&name, &path, &concrete.fields, &mut context)?;
            let expanded: Vec<_> = self
                .dag
                .block_names()
                .into_iter()
                .filter(|node_name| !before.contains(node_name))
                .collect();
            for node_name in expanded {
                let fields = self
                    .dag
                    .get_node(&node_name)
                    .expect("expanded block exists")
                    .fields
                    .clone();
                let rewritten = self.materialize_fields(&fields, &bound.scope, stack)?;
                self.dag.get_node_mut(&node_name).expect("expanded block exists").fields = rewritten;
            }
            // Internal nodes belong to the same explicit invocation.
            for node_name in self.dag.block_names() {
                if (node_name == name || node_name.starts_with(&format!("{name}.")))
                    && let Some(node) = self.dag.get_node_mut(&node_name)
                {
                    node.explicit = block.explicit;
                }
            }
        } else {
            let resource = self
                .registry
                .get_resource(&block.provider, &block.resource)
                .ok_or_else(|| LoadError::UnknownResource {
                    pos: block.pos.clone(),
                    provider: block.provider.clone(),
                    resource: block.resource.clone(),
                })?;
            let prior_state = self.store.load(&name)?;
            self.dag.add_node(DagNode {
                pos: block.pos.clone(),
                name: name.clone(),
                doc: block.doc.clone(),
                phase: block.phase,
                provider: block.provider.clone(),
                resource_name: block.resource.clone(),
                protected: block.protected,
                explicit: block.explicit,
                concurrency_group: block.name.clone(),
                fields: concrete.fields.clone(),
                resource,
                prior_state,
            })?;
            self.scope
                .define(&name, SymbolKind::Block, Value::strct(Map::new()))
                .map_err(|existing| LoadError::DuplicateName {
                    pos: block.pos.clone(),
                    name: name.clone(),
                    existing: existing.as_str(),
                })?;
        }
        stack.pop();
        Ok(name)
    }

    fn materialize_fields(
        &mut self,
        fields: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<Vec<Field>, LoadError> {
        fields
            .iter()
            .map(|field| {
                Ok(Field {
                    name: field.name.clone(),
                    value: self.materialize_expr(&field.value, caller, stack)?,
                })
            })
            .collect()
    }

    fn materialize_expr(
        &mut self,
        expression: &Expr,
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<Expr, LoadError> {
        Ok(match expression {
            Expr::BlockCall {
                name,
                keys,
                args,
                fields,
            } => {
                let block = self
                    .blocks
                    .get(name)
                    .cloned()
                    .ok_or_else(|| invalid(&crate::ast::Pos::default(), name, "unknown parameterized block"))?;
                let instance = if let Some(keys) = keys {
                    self.select_matrix_slice(&block, keys, args, caller, stack)?
                } else {
                    let rewritten_args = self.materialize_fields(args, caller, stack)?;
                    self.instantiate_block(&block, &rewritten_args, caller, stack)?
                };
                if fields.is_empty() {
                    Expr::BlockRef(instance)
                } else {
                    let mut parts = Vec::with_capacity(fields.len() + 1);
                    parts.push(instance);
                    parts.extend(fields.iter().cloned());
                    Expr::Ref(parts)
                }
            }
            Expr::Str(parts) => Expr::Str(
                parts
                    .iter()
                    .map(|part| match part {
                        crate::ast::StringPart::Literal(_) => Ok(part.clone()),
                        crate::ast::StringPart::Interpolation(inner) => Ok(crate::ast::StringPart::Interpolation(
                            self.materialize_expr(inner, caller, stack)?,
                        )),
                    })
                    .collect::<Result<_, LoadError>>()?,
            ),
            Expr::List(items) => Expr::List(
                items
                    .iter()
                    .map(|item| self.materialize_expr(item, caller, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Map(fields) => Expr::Map(
                fields
                    .iter()
                    .map(|field| {
                        Ok(crate::ast::MapEntry {
                            name: field.name.clone(),
                            typ: field.typ.clone(),
                            value: self.materialize_expr(&field.value, caller, stack)?,
                        })
                    })
                    .collect::<Result<_, LoadError>>()?,
            ),
            Expr::MatrixRef { name, keys, fields } => {
                if self.blocks.get(name).is_some_and(|block| !block.params.is_empty()) {
                    return Err(invalid(
                        &crate::ast::Pos::default(),
                        name,
                        "matrix block requires arguments",
                    ));
                }
                Expr::MatrixRef {
                    name: name.clone(),
                    keys: keys
                        .iter()
                        .map(|key| self.materialize_expr(key, caller, stack))
                        .collect::<Result<_, _>>()?,
                    fields: fields.clone(),
                }
            }
            Expr::Call(name, args) => Expr::Call(
                name.clone(),
                args.iter()
                    .map(|arg| self.materialize_expr(arg, caller, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Pipe(inner, name, args) => Expr::Pipe(
                Box::new(self.materialize_expr(inner, caller, stack)?),
                name.clone(),
                args.iter()
                    .map(|arg| self.materialize_expr(arg, caller, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::If(condition, then_value, else_value) => Expr::If(
                Box::new(self.materialize_expr(condition, caller, stack)?),
                Box::new(self.materialize_expr(then_value, caller, stack)?),
                Box::new(self.materialize_expr(else_value, caller, stack)?),
            ),
            Expr::BinOp(left, operator, right) => Expr::BinOp(
                Box::new(self.materialize_expr(left, caller, stack)?),
                operator.clone(),
                Box::new(self.materialize_expr(right, caller, stack)?),
            ),
            Expr::Add(left, right) => Expr::Add(
                Box::new(self.materialize_expr(left, caller, stack)?),
                Box::new(self.materialize_expr(right, caller, stack)?),
            ),
            Expr::Ref(_) | Expr::BlockRef(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Duration(_) | Expr::Null => {
                expression.clone()
            }
        })
    }
}

fn bind(
    name: &str,
    pos: &crate::ast::Pos,
    params: &[Param],
    args: &[Field],
    caller: &Scope,
) -> Result<Bound, LoadError> {
    let mut given = HashMap::new();
    for arg in args {
        if given.insert(arg.name.as_str(), &arg.value).is_some() {
            return Err(invalid(pos, name, &format!("duplicate argument '{}'", arg.name)));
        }
    }
    for key in given.keys() {
        if !params.iter().any(|param| &param.name.as_str() == key) {
            return Err(invalid(pos, name, &format!("unknown argument '{key}'")));
        }
    }
    let mut scope = caller.clone();
    let mut values = Vec::new();
    let mut substitutions = HashMap::new();
    for param in params {
        let value = match given.get(param.name.as_str()) {
            Some(value) => expr::eval(value, caller).map_err(|source| LoadError::Eval {
                pos: pos.clone(),
                source,
            })?,
            None => {
                let default = param
                    .default
                    .as_ref()
                    .ok_or_else(|| invalid(pos, name, &format!("missing required argument '{}'", param.name)))?;
                expr::eval(default, &scope).map_err(|source| LoadError::Eval {
                    pos: pos.clone(),
                    source,
                })?
            }
        };
        validate_type(&value, &param.typ).map_err(|message| LoadError::TypeError {
            pos: pos.clone(),
            name: param.name.clone(),
            message,
        })?;
        scope.set(&param.name, value.clone());
        substitutions.insert(param.name.clone(), value.to_expr());
        values.push(value);
    }
    Ok(Bound {
        scope,
        values,
        substitutions,
    })
}

fn cli_args(
    params: &[Param],
    raw: &[(String, String)],
    name: &str,
    pos: &crate::ast::Pos,
) -> Result<Vec<Field>, LoadError> {
    let mut args = Vec::new();
    for (key, value) in raw {
        let param = params
            .iter()
            .find(|param| &param.name == key)
            .ok_or_else(|| invalid(pos, name, &format!("unknown argument '{key}'")))?;
        let parsed = match &param.typ {
            Type::String | Type::Path | Type::Secret => Value::Str(value.clone()),
            _ => {
                let source = format!("param value : {} = {value}", param.typ);
                let module = crate::parser::parse(&source, "<argument>")
                    .map_err(|error| invalid(pos, name, &format!("invalid value for '{key}': {}", error.message)))?;
                let Some(Statement::Param(parsed)) = module.statements.first() else {
                    return Err(invalid(pos, name, &format!("invalid value for '{key}'")));
                };
                let Some(expression) = &parsed.default else {
                    return Err(invalid(pos, name, &format!("invalid value for '{key}'")));
                };
                expr::eval(expression, &Scope::new()).map_err(|error| invalid(pos, name, &error.to_string()))?
            }
        };
        args.push(Field {
            name: key.clone(),
            value: parsed.to_expr(),
        });
    }
    Ok(args)
}

fn instance_name(name: &str, params: &[Param], values: &[Value]) -> Result<String, LoadError> {
    if values.is_empty() {
        return Ok(name.to_owned());
    }
    let literals: Result<Vec<_>, _> = params
        .iter()
        .zip(values)
        .map(|(param, value)| {
            if param.typ == Type::Secret {
                let encoded = serde_json::to_vec(value)?;
                Ok(format!(r#""secret:{}""#, crate::sha256::SHA256::digest(&encoded)))
            } else {
                serde_json::to_string(value)
            }
        })
        .collect();
    let literals = literals.map_err(|error| invalid(&crate::ast::Pos::default(), name, &error.to_string()))?;
    Ok(format!("{name}[{}]", literals.join(", ")))
}

fn invalid(pos: &crate::ast::Pos, name: &str, message: &str) -> LoadError {
    LoadError::InvalidInvocation {
        pos: pos.clone(),
        name: name.to_owned(),
        message: message.to_owned(),
    }
}
