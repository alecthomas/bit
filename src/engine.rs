use crate::dag::{Dag, DagError};
use crate::expr::{self, EvalError, Scope};

use crate::loader::BaseScope;
use crate::output::{Event, Output};
use crate::provider::{BoxError, PlanAction, PlanResult, ResourceKind};
use crate::state::{StateError, StateStore};
use crate::value::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("eval error in block '{block}': {source}")]
    Eval { block: String, source: EvalError },
    #[error("block '{block}' {phase} failed: {source}")]
    Provider {
        block: String,
        phase: &'static str,
        source: BoxError,
    },
    #[error("{0}")]
    State(#[from] StateError),
    #[error("protected block '{0}' cannot be {1}")]
    Protected(String, &'static str),
    #[error("test block '{0}' failed")]
    TestFailed(String),
}

/// Result of planning a single block.
pub struct BlockPlan {
    pub name: String,
    pub plan: PlanResult,
}

fn plan_action_to_event(action: &PlanAction) -> Event {
    match action {
        PlanAction::Create => Event::Create,
        PlanAction::Update => Event::Update,
        PlanAction::Replace => Event::Replace,
        PlanAction::Destroy => Event::Destroy,
        PlanAction::None => Event::NoChange,
    }
}

/// Plan all blocks in the DAG (or a target subset), returning what would change.
pub fn plan(
    dag: &mut Dag,
    base: &BaseScope,
    output: &Output,
    target: Option<&str>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = match target {
        Some(t) => dag.target_order(t)?,
        None => dag.topo_order()?,
    };

    let mut scope = base.scope.clone();
    let mut plans = Vec::new();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let result = node
            .resource
            .plan(&inputs, node.prior_state.as_ref())
            .map_err(|e| EngineError::Provider {
                block: name.clone(),
                phase: "plan",
                source: e,
            })?;

        if node.protected && matches!(result.action, PlanAction::Replace | PlanAction::Destroy) {
            return Err(EngineError::Protected(
                name.clone(),
                match result.action {
                    PlanAction::Replace => "replaced",
                    PlanAction::Destroy => "destroyed",
                    _ => unreachable!(),
                },
            ));
        }

        writer.event(plan_action_to_event(&result.action), &result.description);

        scope.set(name, Value::Map(Map::new()));

        plans.push(BlockPlan {
            name: name.clone(),
            plan: result,
        });
    }

    Ok(plans)
}

/// Apply all blocks in the DAG (or a target subset).
/// Walks topo order: evaluate fields, resolve, plan, apply, persist state,
/// inject outputs into scope for downstream blocks.
pub fn apply(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    target: Option<&str>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = match target {
        Some(t) => dag.target_order(t)?,
        None => dag.topo_order()?,
    };

    let mut scope = base.scope.clone();
    let mut results = Vec::new();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let plan_result =
            node.resource
                .plan(&inputs, node.prior_state.as_ref())
                .map_err(|e| EngineError::Provider {
                    block: name.clone(),
                    phase: "plan",
                    source: e,
                })?;

        if node.protected && matches!(plan_result.action, PlanAction::Replace | PlanAction::Destroy) {
            return Err(EngineError::Protected(
                name.clone(),
                match plan_result.action {
                    PlanAction::Replace => "replaced",
                    PlanAction::Destroy => "destroyed",
                    _ => unreachable!(),
                },
            ));
        }

        if plan_result.action == PlanAction::None {
            writer.event(Event::Skipped, "no changes");
            scope.set(name, Value::Map(Map::new()));
            results.push(BlockPlan {
                name: name.clone(),
                plan: plan_result,
            });
            continue;
        }

        writer.event(Event::Starting, &plan_result.description);

        let apply_result = node
            .resource
            .apply(&inputs, node.prior_state.as_ref(), &writer)
            .map_err(|e| {
                writer.event(Event::Failed, &e.to_string());
                EngineError::Provider {
                    block: name.clone(),
                    phase: "apply",
                    source: e,
                }
            })?;

        // Persist state
        if let Some(state) = &apply_result.state {
            store.save(name, state)?;
        }

        // Check test blocks
        if node.resource.kind() == ResourceKind::Test
            && let Some(passed) = apply_result.outputs.get("passed").and_then(|v| v.as_bool())
            && !passed
        {
            writer.event(Event::Failed, "tests failed");
            return Err(EngineError::TestFailed(name.clone()));
        }

        writer.event(Event::Done, "");

        // Inject outputs into scope for downstream blocks
        scope.set(name, Value::Map(apply_result.outputs));

        results.push(BlockPlan {
            name: name.clone(),
            plan: plan_result,
        });
    }

    Ok(results)
}

/// Destroy blocks in reverse dependency order.
pub fn destroy(
    dag: &mut Dag,
    store: &dyn StateStore,
    output: &Output,
    target: Option<&str>,
) -> Result<(), EngineError> {
    let mut order = match target {
        Some(t) => dag.target_order(t)?,
        None => dag.topo_order()?,
    };
    order.reverse();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        if node.protected {
            writer.event(Event::Skipped, "protected");
            continue;
        }

        let Some(prior_state) = &node.prior_state else {
            writer.event(Event::Skipped, "no state");
            continue;
        };

        writer.event(Event::Starting, "destroying");

        node.resource.destroy(prior_state, &writer).map_err(|e| {
            writer.event(Event::Failed, &e.to_string());
            EngineError::Provider {
                block: name.clone(),
                phase: "destroy",
                source: e,
            }
        })?;

        store.remove(name)?;
        writer.event(Event::Done, "");
    }

    Ok(())
}

fn eval_fields(fields: &[crate::ast::Field], scope: &Scope) -> Result<Map, EvalError> {
    let mut inputs = Map::new();
    for field in fields {
        let value = expr::eval(&field.value, scope)?;
        inputs.insert(field.name.clone(), value);
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader;
    use crate::parser;
    use crate::provider::ProviderRegistry;
    use crate::providers::exec::ExecProvider;
    use crate::state::StateStore;

    fn test_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(ExecProvider));
        reg
    }

    struct MemoryStore {
        data: std::cell::RefCell<std::collections::HashMap<String, serde_json::Value>>,
    }

    impl MemoryStore {
        fn new() -> Self {
            Self {
                data: std::cell::RefCell::new(std::collections::HashMap::new()),
            }
        }
    }

    impl StateStore for MemoryStore {
        fn load(&self, block: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(self.data.borrow().get(block).cloned())
        }
        fn save(&self, block: &str, state: &serde_json::Value) -> Result<(), StateError> {
            self.data.borrow_mut().insert(block.into(), state.clone());
            Ok(())
        }
        fn remove(&self, block: &str) -> Result<(), StateError> {
            self.data.borrow_mut().remove(block);
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(self.data.borrow().keys().cloned().collect())
        }
    }

    fn load_and_apply(input: &str) -> Result<Vec<BlockPlan>, EngineError> {
        let module = parser::parse(input).expect("parse failed");
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).expect("load failed");
        let output = Output::new(&[]);
        apply(&mut dag, &base, &store, &output, None)
    }

    #[test]
    fn apply_simple_block() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let results = load_and_apply(&input).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "build");
        assert_eq!(results[0].plan.action, PlanAction::Create);
        assert!(output.exists());
    }

    #[test]
    fn apply_chain_passes_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp ${{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let results = load_and_apply(&input).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn plan_shows_actions() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hi\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        let plans = plan(&mut dag, &base, &Output::new(&[]), None).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].plan.action, PlanAction::Create);
    }

    #[test]
    fn destroy_removes_state() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();

        // Apply first
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        apply(&mut dag, &base, &store, &Output::new(&[]), None).unwrap();
        assert!(!store.list().unwrap().is_empty());

        // Reload with state, then destroy
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        destroy(&mut dag, &store, &Output::new(&[]), None).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn protected_block_skips_destroy() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        apply(&mut dag, &base, &store, &Output::new(&[]), None).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        destroy(&mut dag, &store, &Output::new(&[]), None).unwrap();
        // State should still exist — destroy was skipped
        assert!(!store.list().unwrap().is_empty());
    }

    #[test]
    fn target_filters_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let out_a = dir.path().join("a.txt");
        let out_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "target just_a = [a]\n",
            ),
            out_a.display(),
            out_a.display(),
            out_b.display(),
            out_b.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        let results = apply(&mut dag, &base, &store, &Output::new(&[]), Some("just_a")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "a");
        assert!(out_a.exists());
        assert!(!out_b.exists());
    }
}
