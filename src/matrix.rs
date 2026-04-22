use std::collections::HashMap;

use crate::ast::{Block, Expr, Field, StringPart};
use crate::dag::{Dag, DagNode, collect_after, collect_block_refs, collect_depends_on};
use crate::expr::Scope;
use crate::loader::LoadError;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::value::{Map, Value};

/// Format a matrix state key: `name[val1, val2]`.
fn matrix_key(name: &str, values: &[&Value]) -> String {
    let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    format!("{name}[{}]", parts.join(", "))
}

/// Compute the cartesian product of multiple lists.
/// Returns a vec of vecs, where each inner vec has one element per input list.
fn cartesian_product<'a>(lists: &[&'a [Value]]) -> Vec<Vec<&'a Value>> {
    let mut result: Vec<Vec<&'a Value>> = vec![vec![]];
    for list in lists {
        let mut new_result = Vec::new();
        for combo in &result {
            for item in *list {
                let mut new_combo = combo.clone();
                new_combo.push(item);
                new_result.push(new_combo);
            }
        }
        result = new_result;
    }
    result
}

/// Expand a matrix block into individual DagNodes — one per cartesian
/// product combination of its key values.
///
/// Within each expanded block:
/// - Matrix key variables are substituted with their concrete scalar values
/// - References to other matrix blocks sharing the same keys are rewritten
///   to point to the matching slice
pub fn expand_matrix(
    block: &Block,
    scope: &mut Scope,
    registry: &ProviderRegistry,
    store: &dyn StateStore,
    dag: &mut Dag,
    matrix_blocks: &HashMap<String, Vec<String>>,
) -> Result<(), LoadError> {
    // Look up each key — must be a list in scope
    let mut key_lists: Vec<Vec<Value>> = Vec::new();
    for key in &block.matrix_keys {
        let val = scope.get(key).ok_or_else(|| LoadError::MatrixKeyNotFound {
            pos: block.pos.clone(),
            name: key.clone(),
        })?;
        let list = val.as_list().ok_or_else(|| LoadError::MatrixKeyNotList {
            pos: block.pos.clone(),
            name: key.clone(),
        })?;
        key_lists.push(list.to_vec());
    }

    // Compute cartesian product
    let list_refs: Vec<&[Value]> = key_lists.iter().map(|l| l.as_slice()).collect();
    let combos = cartesian_product(&list_refs);

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

    // Track all expanded node names for this block
    let mut expanded_names: Vec<String> = Vec::new();

    for combo in &combos {
        let node_name = matrix_key(&block.name, combo);

        // Build substitution map: key_name -> scalar value
        let subs: HashMap<String, Expr> = block
            .matrix_keys
            .iter()
            .zip(combo.iter())
            .map(|(k, v)| (k.clone(), (*v).to_expr()))
            .collect();

        // Build a map of sibling matrix block names to their expanded names
        // for this combination. Only for blocks sharing the same key set.
        let mut block_subs: HashMap<String, String> = HashMap::new();
        for (other_name, other_keys) in matrix_blocks {
            if other_name == &block.name {
                continue;
            }
            if other_keys == &block.matrix_keys {
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
            fields: rewritten_fields,
            resource,
            prior_state,
        })?;

        expanded_names.push(node_name);
    }

    // Wire dependency edges for expanded nodes
    for expanded in &expanded_names {
        let node_fields = &dag.get_node(expanded).expect("just added").fields.clone();

        for dep in collect_block_refs(node_fields) {
            if dag.has_block(&dep) && dep != *expanded {
                dag.add_dep_edge(&dep, expanded)?;
            }
        }
        for dep in collect_depends_on(node_fields) {
            if dag.has_block(&dep) && dep != *expanded {
                dag.add_dep_edge(&dep, expanded)?;
            }
        }
        for dep in collect_after(node_fields) {
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
        let key: String = if combo.len() == 1 {
            combo[0].to_string()
        } else {
            combo
                .iter()
                .map(|v: &&Value| v.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        };
        matrix_map.insert(key, Value::strct(Map::new()));
    }
    scope
        .define(&block.name, crate::expr::SymbolKind::Block, Value::strct(matrix_map))
        .map_err(|existing| LoadError::DuplicateName {
            pos: block.pos.clone(),
            name: block.name.clone(),
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
                .map(|f| Field {
                    name: f.name.clone(),
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

    #[test]
    fn cartesian_single_list() {
        let a = vec![Value::Str("x".into()), Value::Str("y".into())];
        let result = cartesian_product(&[&a]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn cartesian_two_lists() {
        let a = vec![Value::Str("a".into()), Value::Str("b".into())];
        let b = vec![Value::Str("1".into()), Value::Str("2".into())];
        let result = cartesian_product(&[&a, &b]);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn matrix_key_single() {
        let v = Value::Str("amd64".into());
        assert_eq!(matrix_key("image", &[&v]), "image[amd64]");
    }

    #[test]
    fn matrix_key_multi() {
        let a = Value::Str("amd64".into());
        let b = Value::Str("us".into());
        assert_eq!(matrix_key("deploy", &[&a, &b]), "deploy[amd64, us]");
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
        let block_subs = HashMap::from([("image".to_owned(), "image[amd64]".to_owned())]);

        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        let result = rewrite_matrix_expr(&expr, &subs, &block_subs);
        assert_eq!(result, Expr::Ref(vec!["image[amd64]".into(), "ref".into()]));
    }
}
