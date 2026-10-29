use std::collections::HashMap;

use crate::ast::{Block, Expr, Field, MatrixKey, StringPart};
use crate::dag::{AFTER_FIELD, DEPENDS_ON_FIELD, Dag, DagNode, collect_block_refs, collect_dependency_refs};
use crate::expr::{self, Scope};
use crate::loader::LoadError;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::value::{Map, Value};

/// Format a matrix state key: `name[val1, val2]`.
pub(crate) fn matrix_key(name: &str, values: &[&Value]) -> String {
    format!("{name}[{}]", matrix_values_key(values))
}

/// Name one slice of a matrix block instance. A parameterized instance's
/// arguments follow the keys, mirroring the call syntax `name[keys](args)`.
pub(crate) fn slice_name(name: &str, values: &[&Value], instance_args: &str) -> String {
    format!("{}{instance_args}", matrix_key(name, values))
}

/// List the sorted slices of a matrix group, or `None` if `group` is not one.
///
/// Block names cannot contain `(`, so the first one starts an instance's arguments.
pub(crate) fn group_slices(group: &str, scope: &Scope) -> Option<Vec<String>> {
    let Some(Value::Struct(_, slices)) = scope.get(group) else {
        return None;
    };
    let (name, instance_args) = group.split_at(group.find('(').unwrap_or(group.len()));
    let mut names: Vec<_> = slices
        .keys()
        .map(|key| format!("{name}[{key}]{instance_args}"))
        .collect();
    names.sort();
    Some(names)
}

fn matrix_values_key(values: &[&Value]) -> String {
    values
        .iter()
        .map(|value| value.to_literal())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compute every combination of matrix key values, in key declaration order.
///
/// Bare keys form a cartesian product; a values expression is re-evaluated per earlier combination.
fn matrix_combinations(block: &Block, scope: &Scope) -> Result<Vec<Vec<Value>>, LoadError> {
    let mut combos: Vec<Vec<Value>> = vec![vec![]];
    for (index, key) in block.matrix_keys.iter().enumerate() {
        let mut next = Vec::new();
        match &key.values {
            None => {
                let list = scope_key_list(block, key, scope)?;
                for combo in &combos {
                    for value in &list {
                        next.push([combo.as_slice(), std::slice::from_ref(value)].concat());
                    }
                }
            }
            Some(values) => {
                for combo in &combos {
                    let mut key_scope = scope.clone();
                    for (earlier, value) in block.matrix_keys[..index].iter().zip(combo) {
                        key_scope.set(earlier.name.clone(), value.clone());
                    }
                    let list = expr::eval(values, &key_scope).map_err(|source| LoadError::Eval {
                        pos: block.pos.clone(),
                        source,
                    })?;
                    let list = list.as_list().ok_or_else(|| LoadError::MatrixKeyNotList {
                        pos: block.pos.clone(),
                        name: key.name.clone(),
                    })?;
                    for value in list {
                        next.push([combo.as_slice(), std::slice::from_ref(value)].concat());
                    }
                }
            }
        }
        combos = next;
    }
    Ok(combos)
}

fn scope_key_list(block: &Block, key: &MatrixKey, scope: &Scope) -> Result<Vec<Value>, LoadError> {
    let value = scope.get(&key.name).ok_or_else(|| LoadError::MatrixKeyNotFound {
        pos: block.pos.clone(),
        name: key.name.clone(),
    })?;
    let list = value.as_list().ok_or_else(|| LoadError::MatrixKeyNotList {
        pos: block.pos.clone(),
        name: key.name.clone(),
    })?;
    Ok(list.to_vec())
}

/// Expand a matrix block into individual DagNodes — one per combination of
/// its key values.
///
/// Within each expanded block:
/// - Matrix key variables are substituted with their concrete scalar values
/// - References to other matrix blocks sharing the same keys are rewritten
///   to point to the matching slice
///
/// `instance_args` is the argument list of a parameterized instance, or empty.
/// The group is named `name(args)` and each slice `name[keys](args)`.
pub fn expand_matrix(
    block: &Block,
    instance_args: &str,
    scope: &mut Scope,
    registry: &ProviderRegistry,
    store: &dyn StateStore,
    dag: &mut Dag,
    matrix_blocks: &HashMap<String, Vec<String>>,
) -> Result<(), LoadError> {
    let key_names = block.matrix_key_names();
    let combos: Vec<Vec<Value>> = matrix_combinations(block, scope)?;
    let combos: Vec<Vec<&Value>> = combos.iter().map(|combo| combo.iter().collect()).collect();

    // Look up the resource once
    let resource_factory = || {
        registry
            .get_resource(&block.provider, &block.resource)
            .ok_or_else(|| LoadError::UnknownResource {
                pos: block.pos.clone(),
                provider: block.provider.clone(),
                resource: block.resource.clone(),
            })
    };

    let group = format!("{}{instance_args}", block.name);
    // Track all expanded node names for this block
    let mut expanded_names: Vec<String> = Vec::new();

    for combo in &combos {
        let node_name = slice_name(&block.name, combo, instance_args);

        // Build substitution map: key_name -> scalar value
        let subs: HashMap<String, Expr> = key_names
            .iter()
            .zip(combo.iter())
            .map(|(k, v)| (k.clone(), (*v).to_expr()))
            .collect();

        // Build a map of sibling matrix block names to their expanded names
        // for this combination. Only for blocks sharing the same key set.
        let mut block_subs: HashMap<String, String> = HashMap::new();
        for (other_name, other_keys) in matrix_blocks {
            if other_name == &group {
                continue;
            }
            if other_keys == &key_names {
                block_subs.insert(other_name.clone(), matrix_key(other_name, combo));
            }
        }

        // Rewrite field expressions
        let rewritten_fields: Vec<Field> = block
            .fields
            .iter()
            .map(|f| Field {
                name: f.name.clone(),
                value: rewrite_matrix_expr(&f.value, &subs, &block_subs),
            })
            .collect();

        let resource = resource_factory()?;
        let prior_state = store.load(&node_name)?;

        dag.add_node(DagNode {
            pos: block.pos.clone(),
            name: node_name.clone(),
            doc: block.doc.clone(),
            phase: block.phase,
            provider: block.provider.clone(),
            resource_name: block.resource.clone(),
            protected: block.protected,
            explicit: block.explicit,
            large: block.large,
            concurrency_group: group.clone(),
            fields: rewritten_fields,
            resource,
            prior_state,
        })?;

        expanded_names.push(node_name);
    }

    // Wire dependency edges for expanded nodes
    for expanded in &expanded_names {
        let node_fields = &dag.get_node(expanded).expect("just added").fields.clone();

        for dep in collect_block_refs(node_fields, scope).map_err(|source| LoadError::Eval {
            pos: block.pos.clone(),
            source,
        })? {
            if dag.has_block(&dep) && dep != *expanded {
                dag.add_dep_edge(&dep, expanded)?;
            }
        }
        for dep in collect_dependency_refs(node_fields, DEPENDS_ON_FIELD, scope).map_err(|source| LoadError::Eval {
            pos: block.pos.clone(),
            source,
        })? {
            if dag.has_block(&dep) && dep != *expanded {
                dag.add_dep_edge(&dep, expanded)?;
            }
        }
        for dep in collect_dependency_refs(node_fields, AFTER_FIELD, scope).map_err(|source| LoadError::Eval {
            pos: block.pos.clone(),
            source,
        })? {
            if dag.has_block(&dep) && dep != *expanded {
                dag.add_ordering_edge(&dep, expanded)?;
            }
        }
    }

    // Register the block name in scope as a map keyed by matrix values,
    // so non-matrix blocks can reference the matrix block.
    // Each entry is an empty map placeholder (outputs filled in at execution time).
    let mut matrix_map = Map::new();
    for combo in &combos {
        let key = matrix_values_key(combo);
        matrix_map.insert(key, Value::strct(Map::new()));
    }
    scope
        .define(&group, crate::expr::SymbolKind::Block, Value::strct(matrix_map))
        .map_err(|existing| LoadError::DuplicateName {
            pos: block.pos.clone(),
            name: group.clone(),
            existing: existing.as_str(),
        })?;

    for expanded in &expanded_names {
        scope
            .define(expanded, crate::expr::SymbolKind::Block, Value::strct(Map::new()))
            .map_err(|existing| LoadError::DuplicateName {
                pos: block.pos.clone(),
                name: expanded.clone(),
                existing: existing.as_str(),
            })?;
    }

    Ok(())
}

/// Rewrite an expression for a matrix expansion slice.
///
/// - Refs matching matrix key names are substituted with concrete values
/// - Refs matching sibling matrix blocks are rewritten to the expanded name
fn rewrite_matrix_expr(expr: &Expr, key_subs: &HashMap<String, Expr>, block_subs: &HashMap<String, String>) -> Expr {
    match expr {
        Expr::Ref(parts) => {
            let first = &parts[0];
            // Substitute matrix key variables (single-part refs only)
            if parts.len() == 1
                && let Some(sub) = key_subs.get(first)
            {
                return sub.clone();
            }
            // Rewrite references to sibling matrix blocks
            if let Some(expanded_name) = block_subs.get(first) {
                let mut new_parts = parts.clone();
                new_parts[0] = expanded_name.clone();
                return Expr::Ref(new_parts);
            }
            expr.clone()
        }
        Expr::BlockRef(_) => expr.clone(),
        Expr::MatrixRef { name, keys, fields } => Expr::MatrixRef {
            name: name.clone(),
            keys: keys
                .iter()
                .map(|key| rewrite_matrix_expr(key, key_subs, block_subs))
                .collect(),
            fields: fields.clone(),
        },
        Expr::BlockCall {
            name,
            keys,
            args,
            fields,
        } => Expr::BlockCall {
            name: name.clone(),
            keys: keys.as_ref().map(|keys| {
                keys.iter()
                    .map(|key| rewrite_matrix_expr(key, key_subs, block_subs))
                    .collect()
            }),
            args: args
                .iter()
                .map(|arg| Field {
                    name: arg.name.clone(),
                    value: rewrite_matrix_expr(&arg.value, key_subs, block_subs),
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
                        StringPart::Interpolation(rewrite_matrix_expr(e, key_subs, block_subs))
                    }
                })
                .collect();
            Expr::Str(new_parts)
        }
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|e| rewrite_matrix_expr(e, key_subs, block_subs))
                .collect(),
        ),
        Expr::Map(fields) => Expr::Map(
            fields
                .iter()
                .map(|f| crate::ast::MapEntry {
                    name: f.name.clone(),
                    typ: f.typ.clone(),
                    value: rewrite_matrix_expr(&f.value, key_subs, block_subs),
                })
                .collect(),
        ),
        Expr::Call(name, args) => Expr::Call(
            name.clone(),
            args.iter()
                .map(|e| rewrite_matrix_expr(e, key_subs, block_subs))
                .collect(),
        ),
        Expr::Pipe(inner, name, args) => Expr::Pipe(
            Box::new(rewrite_matrix_expr(inner, key_subs, block_subs)),
            name.clone(),
            args.iter()
                .map(|e| rewrite_matrix_expr(e, key_subs, block_subs))
                .collect(),
        ),
        Expr::If(cond, then_val, else_val) => Expr::If(
            Box::new(rewrite_matrix_expr(cond, key_subs, block_subs)),
            Box::new(rewrite_matrix_expr(then_val, key_subs, block_subs)),
            Box::new(rewrite_matrix_expr(else_val, key_subs, block_subs)),
        ),
        Expr::BinOp(lhs, op, rhs) => Expr::BinOp(
            Box::new(rewrite_matrix_expr(lhs, key_subs, block_subs)),
            op.clone(),
            Box::new(rewrite_matrix_expr(rhs, key_subs, block_subs)),
        ),
        Expr::Add(lhs, rhs) => Expr::Add(
            Box::new(rewrite_matrix_expr(lhs, key_subs, block_subs)),
            Box::new(rewrite_matrix_expr(rhs, key_subs, block_subs)),
        ),
        Expr::Number(_) | Expr::Bool(_) | Expr::Duration(_) | Expr::Null => expr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combinations(source: &str, scope: &Scope) -> Vec<String> {
        let module = crate::parser::parse(source, "<test>").unwrap();
        let crate::ast::Statement::Block(block) = &module.statements[0] else {
            panic!("expected block");
        };
        matrix_combinations(block, scope)
            .unwrap()
            .iter()
            .map(|combo| matrix_values_key(&combo.iter().collect::<Vec<_>>()))
            .collect()
    }

    fn strings(values: &[&str]) -> Value {
        Value::list(values.iter().map(|value| Value::Str((*value).into())).collect())
    }

    #[test]
    fn bare_keys_form_cartesian_product() {
        let mut scope = Scope::new();
        scope.set("arch", strings(&["amd64", "arm64"]));
        scope.set("os", strings(&["linux", "darwin"]));
        assert_eq!(
            combinations("job[arch, os] = exec {}", &scope),
            [
                r#""amd64", "linux""#,
                r#""amd64", "darwin""#,
                r#""arm64", "linux""#,
                r#""arm64", "darwin""#
            ]
        );
    }

    #[test]
    fn values_expression_sees_earlier_key_values() {
        let mut scope = Scope::new();
        scope.set("module", strings(&["a", "b"]));
        assert_eq!(
            combinations(
                r##"job[module, package = ["#{module}/x", "#{module}/y"]] = exec {}"##,
                &scope
            ),
            [r#""a", "a/x""#, r#""a", "a/y""#, r#""b", "b/x""#, r#""b", "b/y""#]
        );
    }

    #[test]
    fn values_expression_can_vary_list_length() {
        let mut scope = Scope::new();
        scope.set("group", strings(&["one", "none"]));
        assert_eq!(
            combinations(
                r#"job[group, item = if group == "one" then ["x"] else []] = exec {}"#,
                &scope
            ),
            [r#""one", "x""#]
        );
    }

    #[test]
    fn values_expression_must_be_a_list() {
        let module = crate::parser::parse(r#"job[item = "x"] = exec {}"#, "<test>").unwrap();
        let crate::ast::Statement::Block(block) = &module.statements[0] else {
            panic!("expected block");
        };
        assert!(matches!(
            matrix_combinations(block, &Scope::new()),
            Err(LoadError::MatrixKeyNotList { name, .. }) if name == "item"
        ));
    }

    #[test]
    fn matrix_key_single() {
        let v = Value::Str("amd64".into());
        assert_eq!(matrix_key("image", &[&v]), r#"image["amd64"]"#);
    }

    #[test]
    fn matrix_key_multi() {
        let a = Value::Str("amd64".into());
        let b = Value::Str("us".into());
        assert_eq!(matrix_key("deploy", &[&a, &b]), r#"deploy["amd64", "us"]"#);
    }

    #[test]
    fn matrix_key_preserves_value_types() {
        let string = Value::Str("1.0".into());
        let number = Value::Number(1.into());
        let boolean = Value::Bool(true);

        assert_eq!(
            matrix_key("build", &[&string, &number, &boolean]),
            r#"build["1.0", 1, true]"#
        );
    }

    #[test]
    fn matrix_key_preserves_block_reference_type() {
        let reference = Value::BlockRef(r#"crate["core"]"#.into());

        assert_eq!(matrix_key("consumer", &[&reference]), r#"consumer[crate["core"]]"#);
    }

    #[test]
    fn rewrite_substitutes_key() {
        let subs = HashMap::from([("arch".to_owned(), Expr::Str(vec![StringPart::Literal("amd64".into())]))]);
        let block_subs = HashMap::new();

        let expr = Expr::Ref(vec!["arch".into()]);
        let result = rewrite_matrix_expr(&expr, &subs, &block_subs);
        assert_eq!(result, Expr::Str(vec![StringPart::Literal("amd64".into())]));
    }

    #[test]
    fn rewrite_remaps_sibling_block() {
        let subs = HashMap::new();
        let block_subs = HashMap::from([("image".to_owned(), r#"image["amd64"]"#.to_owned())]);

        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        let result = rewrite_matrix_expr(&expr, &subs, &block_subs);
        assert_eq!(result, Expr::Ref(vec![r#"image["amd64"]"#.into(), "ref".into()]));
    }
}
