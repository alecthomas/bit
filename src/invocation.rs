//! Bind group and target arguments before the graph is wired.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, Field, Group, Module, Param, Statement, Target};
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
    targets: HashMap<String, Target>,
    groups: HashMap<String, Group>,
    matrix_targets: &'a mut HashMap<String, Vec<String>>,
    scope: &'a mut Scope,
    dag: &'a mut Dag,
    registry: &'a ProviderRegistry,
    store: &'a dyn StateStore,
    import_roots: &'a [crate::import::ImportRoot],
}

pub(crate) fn materialize(
    module: &Module,
    invocations: &[Invocation],
    concrete_targets: &mut [Target],
    matrix_targets: &mut HashMap<String, Vec<String>>,
    environment: &mut module::ExpandContext<'_>,
) -> Result<Vec<String>, LoadError> {
    let mut targets = environment.dag.target_declarations().clone();
    targets.extend(module.statements.iter().filter_map(|stmt| match stmt {
        Statement::Target(target) => Some((target.name.clone(), target.clone())),
        _ => None,
    }));
    let mut groups = environment.dag.group_declarations().clone();
    groups.extend(module.statements.iter().filter_map(|stmt| match stmt {
        Statement::Group(group) => Some((group.name.clone(), group.clone())),
        _ => None,
    }));
    let mut context = Instantiator {
        targets,
        groups,
        matrix_targets,
        scope: environment.scope,
        dag: environment.dag,
        registry: environment.registry,
        store: environment.store,
        import_roots: environment.import_roots,
    };

    // Targets already present in the DAG include ordinary targets, matrix
    // slices, and expanded module nodes. Resolve calls in all of them before
    // dependency discovery so the syntax works in every target field.
    let existing = context.dag.target_names();
    for name in existing {
        let fields = context
            .dag
            .get_node(&name)
            .expect("listed target exists")
            .fields
            .clone();
        let caller = context.scope.clone();
        let rewritten = context.materialize_fields(&fields, &caller, &mut Vec::new())?;
        context.dag.get_node_mut(&name).expect("listed target exists").fields = rewritten;
    }
    for target in concrete_targets {
        if !target.matrix_keys.is_empty() {
            continue;
        }
        let caller = context.scope.clone();
        target.fields = context.materialize_fields(&target.fields, &caller, &mut Vec::new())?;
    }

    // Preserve existing validation and listing for groups with no parameters.
    let mut plain_groups: Vec<_> = context
        .groups
        .values()
        .filter(|group| group.params.is_empty())
        .map(|group| group.name.clone())
        .collect();
    plain_groups.sort();
    for name in plain_groups {
        context.instantiate_group(&name, &[], &context.scope.clone(), &mut Vec::new())?;
    }

    if invocations.is_empty()
        && let Some(default) = context.groups.get("default").cloned()
        && !default.params.is_empty()
    {
        let targets = context.instantiate_group("default", &[], &context.scope.clone(), &mut Vec::new())?;
        context.dag.add_group("default".into(), targets, default.doc);
    }

    let mut selected = Vec::new();
    for invocation in invocations {
        if let Some(group) = context.groups.get(&invocation.name).cloned() {
            let args = cli_args(&group.params, &invocation.args, &group.name, &group.pos)?;
            let name = context.group_name(&group, &args, &context.scope.clone())?;
            context.instantiate_group(&group.name, &args, &context.scope.clone(), &mut Vec::new())?;
            selected.push(name);
        } else if let Some(target) = context.targets.get(&invocation.name).cloned() {
            let args = cli_args(&target.params, &invocation.args, &target.name, &target.pos)?;
            let name = context.instantiate_target(&target, &args, &context.scope.clone(), &mut Vec::new())?;
            if target.matrix_keys.is_empty() {
                selected.push(name);
            } else {
                selected.extend(matrix::instance_slices(&name, context.scope).unwrap_or_default());
            }
        } else {
            if !invocation.args.is_empty() {
                return Err(invalid(
                    &crate::ast::Pos::default(),
                    &invocation.name,
                    "unknown group or target",
                ));
            }
            selected.push(context.cli_matrix_slice(&invocation.name));
        }
    }
    Ok(selected)
}

impl Instantiator<'_> {
    /// Resolve a CLI matrix selector whose string keys may be unquoted, so
    /// `build[amd64]` selects `build["amd64"]`. Unlike `.bit` files, the CLI
    /// has no scope in which a bare word could name a binding, so a bare
    /// identifier key is a string and every other key must be a literal.
    fn cli_matrix_slice(&self, selector: &str) -> String {
        if self.dag.has_target(selector) || self.dag.groups().contains_key(selector) {
            return selector.to_owned();
        }
        let Ok(Expr::MatrixRef { name, keys, fields }) = crate::parser::parse_expr(selector, "<selector>") else {
            return selector.to_owned();
        };
        if !fields.is_empty() || !self.matrix_targets.contains_key(&name) {
            return selector.to_owned();
        }
        let values: Option<Vec<Value>> = keys
            .iter()
            .map(|key| match key {
                Expr::Ref(parts) if parts.len() == 1 => Some(Value::Str(parts[0].clone())),
                key => expr::eval(key, &Scope::new()).ok(),
            })
            .collect();
        let Some(values) = values else {
            return selector.to_owned();
        };
        let slice = matrix::matrix_key(&name, &values.iter().collect::<Vec<_>>());
        if self.dag.has_target(&slice) {
            slice
        } else {
            selector.to_owned()
        }
    }

    fn group_name(&self, group: &Group, args: &[Field], caller: &Scope) -> Result<String, LoadError> {
        let bound = bind(&group.name, &group.pos, &group.params, args, caller)?;
        Ok(instance_name(&group.name, &group.params, &bound.values))
    }

    fn instantiate_group(
        &mut self,
        name: &str,
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<Vec<String>, LoadError> {
        let Some(group) = self.groups.get(name).cloned() else {
            if !args.is_empty() {
                return Err(invalid(
                    &crate::ast::Pos::default(),
                    name,
                    "this group does not accept arguments",
                ));
            }
            return self
                .dag
                .groups()
                .get(name)
                .map(|group| group.targets.clone())
                .ok_or_else(|| invalid(&crate::ast::Pos::default(), name, "unknown group"));
        };
        let bound = bind(name, &group.pos, &group.params, args, caller)?;
        let instance = instance_name(name, &group.params, &bound.values);
        if let Some(existing) = self.dag.groups().get(&instance) {
            return Ok(existing.targets.clone());
        }
        if stack.iter().any(|entry| entry == name) {
            return Err(invalid(&group.pos, name, "recursive group invocation"));
        }
        stack.push(name.to_owned());
        let mut targets = Vec::new();
        for call in &group.members {
            if self.groups.contains_key(&call.name) || self.dag.groups().contains_key(&call.name) {
                if call.keys.is_some() {
                    return Err(invalid(&group.pos, &call.name, "a group cannot have matrix keys"));
                }
                targets.extend(self.instantiate_group(&call.name, &call.args, &bound.scope, stack)?);
            } else if let Some(target) = self.targets.get(&call.name).cloned() {
                if let Some(keys) = &call.keys {
                    targets.push(self.select_matrix_slice(&target, keys, &call.args, &bound.scope, &mut Vec::new())?);
                } else {
                    let name = self.instantiate_target(&target, &call.args, &bound.scope, &mut Vec::new())?;
                    if target.matrix_keys.is_empty() {
                        targets.push(name);
                    } else {
                        targets.extend(matrix::instance_slices(&name, self.scope).unwrap_or_default());
                    }
                }
            } else {
                if !call.args.is_empty() || call.keys.is_some() {
                    return Err(invalid(&group.pos, &call.name, "unknown target or group"));
                }
                targets.push(call.name.clone());
            }
        }
        stack.pop();
        self.dag.add_group(instance, targets.clone(), group.doc);
        Ok(targets)
    }

    fn select_matrix_slice(
        &mut self,
        target: &Target,
        keys: &[Expr],
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<String, LoadError> {
        if target.matrix_keys.is_empty() || keys.len() != target.matrix_keys.len() {
            return Err(invalid(&target.pos, &target.name, "invalid matrix slice"));
        }
        let rewritten_keys: Vec<_> = keys
            .iter()
            .map(|key| self.materialize_expr(key, caller, stack))
            .collect::<Result<_, _>>()?;
        let rewritten_args = self.materialize_fields(args, caller, stack)?;
        let instance = self.instantiate_target(target, &rewritten_args, caller, stack)?;
        let values: Vec<_> = rewritten_keys
            .iter()
            .map(|key| {
                expr::eval(key, caller).map_err(|source| LoadError::Eval {
                    pos: target.pos.clone(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?;
        let value_refs: Vec<_> = values.iter().collect();
        let slice = matrix::slice_name(&target.name, &value_refs, &instance[target.name.len()..]);
        if !self.dag.has_target(&slice) {
            return Err(invalid(
                &target.pos,
                &target.name,
                &format!("unknown matrix slice '{slice}'"),
            ));
        }
        Ok(slice)
    }

    fn instantiate_target(
        &mut self,
        target: &Target,
        args: &[Field],
        caller: &Scope,
        stack: &mut Vec<String>,
    ) -> Result<String, LoadError> {
        if target.params.is_empty() {
            if !args.is_empty() {
                return Err(invalid(
                    &target.pos,
                    &target.name,
                    "this target does not accept arguments",
                ));
            }
            return Ok(target.name.clone());
        }
        let bound = bind(&target.name, &target.pos, &target.params, args, caller)?;
        let instance_args = instance_args(&target.params, &bound.values);
        let name = format!("{}{instance_args}", target.name);
        if self.dag.has_target(&name) || self.matrix_targets.contains_key(&name) {
            return Ok(name);
        }
        if stack.iter().any(|entry| entry == &target.name) {
            return Err(invalid(&target.pos, &target.name, "recursive target invocation"));
        }
        stack.push(target.name.clone());
        let mut concrete = target.clone();
        concrete.name = name.clone();
        concrete.params.clear();
        concrete.explicit = target.explicit;
        let substitutions: HashMap<_, _> = bound
            .substitutions
            .iter()
            .filter(|(key, _)| !target.matrix_keys.iter().any(|matrix_key| &matrix_key.name == *key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let substituted: Vec<Field> = target
            .fields
            .iter()
            .map(|field| Field {
                name: field.name.clone(),
                value: module::rewrite_expr(&field.value, &HashSet::new(), &substitutions, ""),
            })
            .collect();
        if !target.matrix_keys.is_empty() {
            concrete.name = target.name.clone();
            concrete.fields = substituted;
            // Matrix keys may be bound target parameters. Expand in that scope,
            // then publish only the resulting nodes to the module scope.
            let mut expansion_scope = bound.scope.clone();
            let before: HashSet<_> = self.dag.target_names().into_iter().collect();
            matrix::expand_matrix(
                &concrete,
                &instance_args,
                &mut expansion_scope,
                self.registry,
                self.store,
                self.dag,
                self.matrix_targets,
            )?;
            let value = expansion_scope.get(&name).cloned().ok_or_else(|| {
                invalid(
                    &target.pos,
                    &target.name,
                    "matrix expansion did not register its instance",
                )
            })?;
            self.scope
                .define(&name, SymbolKind::Target, value)
                .map_err(|existing| LoadError::DuplicateName {
                    pos: target.pos.clone(),
                    name: name.clone(),
                    existing: existing.as_str(),
                })?;
            let expanded: Vec<_> = self
                .dag
                .target_names()
                .into_iter()
                .filter(|node_name| !before.contains(node_name))
                .collect();
            let invocation = cli_invocation(target, &bound.values, self.scope);
            for node_name in expanded {
                // Selecting the instance selects every slice.
                self.dag.add_invocation(node_name.clone(), invocation.clone());
                self.scope
                    .define(&node_name, SymbolKind::Target, Value::strct(Map::new()))
                    .map_err(|existing| LoadError::DuplicateName {
                        pos: target.pos.clone(),
                        name: node_name.clone(),
                        existing: existing.as_str(),
                    })?;
                let fields = self
                    .dag
                    .get_node(&node_name)
                    .ok_or_else(|| invalid(&target.pos, &target.name, "matrix slice was not registered"))?
                    .fields
                    .clone();
                let rewritten = self.materialize_fields(&fields, &expansion_scope, stack)?;
                self.dag
                    .get_node_mut(&node_name)
                    .ok_or_else(|| invalid(&target.pos, &target.name, "matrix slice was not registered"))?
                    .fields = rewritten;
            }
            self.matrix_targets.insert(name.clone(), target.matrix_key_names());
            stack.pop();
            return Ok(name);
        }
        concrete.fields = self.materialize_fields(&substituted, &bound.scope, stack)?;

        if let Some(path) = module::resolve_module_path(self.import_roots, &target.provider, &target.resource) {
            let before: HashSet<_> = self.dag.target_names().into_iter().collect();
            let mut context = module::ExpandContext {
                scope: self.scope,
                registry: self.registry,
                store: self.store,
                dag: self.dag,
                import_roots: self.import_roots,
            };
            module::expand_module(
                &name,
                &path,
                &concrete.fields,
                module::InstanceModifiers::of(target),
                &mut context,
            )?;
            let expanded: Vec<_> = self
                .dag
                .target_names()
                .into_iter()
                .filter(|node_name| !before.contains(node_name))
                .collect();
            for node_name in expanded {
                let fields = self
                    .dag
                    .get_node(&node_name)
                    .expect("expanded target exists")
                    .fields
                    .clone();
                let rewritten = self.materialize_fields(&fields, &bound.scope, stack)?;
                self.dag
                    .get_node_mut(&node_name)
                    .expect("expanded target exists")
                    .fields = rewritten;
            }
            // Internal nodes belong to the same explicit invocation.
            for node_name in self.dag.target_names() {
                if (node_name == name || node_name.starts_with(&format!("{name}.")))
                    && let Some(node) = self.dag.get_node_mut(&node_name)
                {
                    node.explicit = target.explicit;
                }
            }
        } else {
            let resource = self
                .registry
                .get_resource(&target.provider, &target.resource)
                .ok_or_else(|| LoadError::UnknownResource {
                    pos: target.pos.clone(),
                    provider: target.provider.clone(),
                    resource: target.resource.clone(),
                })?;
            let prior_state = self.store.load(&name)?;
            self.dag.add_node(DagNode {
                pos: target.pos.clone(),
                name: name.clone(),
                doc: target.doc.clone(),
                phase: target.phase,
                provider: target.provider.clone(),
                resource_name: target.resource.clone(),
                protected: target.protected,
                explicit: target.explicit,
                large: target.large,
                concurrency_group: target.name.clone(),
                fields: concrete.fields.clone(),
                resource,
                prior_state,
            })?;
            self.dag
                .add_invocation(name.clone(), cli_invocation(target, &bound.values, self.scope));
            self.scope
                .define(&name, SymbolKind::Target, Value::strct(Map::new()))
                .map_err(|existing| LoadError::DuplicateName {
                    pos: target.pos.clone(),
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
            Expr::TargetCall {
                name,
                keys,
                args,
                fields,
            } => {
                let target = self
                    .targets
                    .get(name)
                    .cloned()
                    .ok_or_else(|| invalid(&crate::ast::Pos::default(), name, "unknown parameterized target"))?;
                let instance = if let Some(keys) = keys {
                    self.select_matrix_slice(&target, keys, args, caller, stack)?
                } else {
                    let rewritten_args = self.materialize_fields(args, caller, stack)?;
                    self.instantiate_target(&target, &rewritten_args, caller, stack)?
                };
                if fields.is_empty() {
                    Expr::TargetRef(instance)
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
                if self.targets.get(name).is_some_and(|target| !target.params.is_empty()) {
                    return Err(invalid(
                        &crate::ast::Pos::default(),
                        name,
                        "matrix target requires arguments",
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
            Expr::Ref(_) | Expr::TargetRef(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Duration(_) | Expr::Null => {
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
        // Bound values name the instance, and an instance must be writable
        // back as a CLI invocation, which never carries secrets.
        if value.contains_secret() {
            return Err(invalid(
                pos,
                name,
                &format!(
                    "parameter '{}' cannot receive a secret; read it with secret(...) inside the target",
                    param.name
                ),
            ));
        }
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
            Type::String | Type::Path => Value::Str(value.clone()).to_expr(),
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
                expr::eval(expression, &Scope::new())
                    .map_err(|error| invalid(pos, name, &error.to_string()))?
                    .to_expr()
            }
        };
        args.push(Field {
            name: key.clone(),
            value: parsed,
        });
    }
    Ok(args)
}

/// The command-line invocation that selects an instance, the inverse of
/// [`cli_args`]. It leaves out arguments that equal their default.
fn cli_invocation(target: &Target, values: &[Value], module_scope: &Scope) -> Invocation {
    // Evaluate defaults as `bind` does for a command-line invocation, so an
    // argument left out binds to the same value.
    let mut scope = module_scope.clone();
    let mut args = Vec::new();
    for (param, value) in target.params.iter().zip(values) {
        let default = param
            .default
            .as_ref()
            .and_then(|default| expr::eval(default, &scope).ok());
        if default.as_ref() != Some(value) {
            let text = match (&param.typ, value) {
                (Type::String | Type::Path, Value::Str(text)) => text.clone(),
                _ => value.to_literal(),
            };
            args.push((param.name.clone(), text));
        }
        scope.set(&param.name, value.clone());
    }
    Invocation {
        name: target.name.clone(),
        args,
    }
}

fn instance_name(name: &str, params: &[Param], values: &[Value]) -> String {
    format!("{name}{}", instance_args(params, values))
}

/// Format bound values as the named argument list of a call, such as
/// `(tag="1.2.3")`, so instance names read like the calls that create them.
/// Empty when there are no values.
fn instance_args(params: &[Param], values: &[Value]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let arguments: Vec<_> = params
        .iter()
        .zip(values)
        .map(|(param, value)| format!("{}={}", param.name, value.to_literal()))
        .collect();
    format!("({})", arguments.join(", "))
}

fn invalid(pos: &crate::ast::Pos, name: &str, message: &str) -> LoadError {
    LoadError::InvalidInvocation {
        pos: pos.clone(),
        name: name.to_owned(),
        message: message.to_owned(),
    }
}
