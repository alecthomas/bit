use sha2::{Digest, Sha256};

use crate::dag::{Dag, DagError};
use crate::expr::{self, EvalError, Scope};
use crate::loader::BaseScope;
use crate::output::{Event, Output};
use crate::provider::{BoxError, PlanAction, PlanResult, ResolvedFile, ResourceKind};
use crate::providers::hash_file;
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

/// Wrapped state persisted by the engine. Contains the provider's own state,
/// outputs, and a combined hash of all inputs (resolved files + parent states).
#[derive(serde::Serialize, serde::Deserialize)]
struct WrappedState {
    state: serde_json::Value,
    outputs: Map,
    content_hash: String,
}

/// Extract the provider state, outputs, and stored content hash from persisted state.
fn unwrap_state(stored: &serde_json::Value) -> (Option<serde_json::Value>, Map, String) {
    let wrapped: WrappedState =
        serde_json::from_value(stored.clone()).expect("corrupted state: not a valid WrappedState");
    (Some(wrapped.state), wrapped.outputs, wrapped.content_hash)
}

/// Cache of file path -> content hash, shared across all blocks in a run.
type HashCache = std::collections::HashMap<std::path::PathBuf, String>;

/// Compute a combined hash of files and parent block states.
fn compute_content_hash(
    files: &[std::path::PathBuf],
    dag: &Dag,
    block_name: &str,
    store: &dyn StateStore,
    cache: &mut HashCache,
) -> String {
    let mut hasher = Sha256::new();

    // Hash files (sorted for determinism)
    let mut sorted = files.to_vec();
    sorted.sort();
    for file in &sorted {
        let hash = cache
            .entry(file.clone())
            .or_insert_with(|| hash_file(file).unwrap_or_default());
        if !hash.is_empty() {
            hasher.update(file.to_string_lossy().as_bytes());
            hasher.update(hash.as_bytes());
        }
    }

    // Hash parent block states (content-coupled deps only)
    let mut deps = dag.content_deps(block_name);
    deps.sort();
    for dep in &deps {
        if let Ok(Some(state)) = store.load(dep) {
            hasher.update(dep.as_bytes());
            hasher.update(state.to_string().as_bytes());
        }
    }

    format!("sha256:{:x}", hasher.finalize())
}

/// Expand resolved file entries into concrete file paths.
/// InputGlob patterns are expanded via filesystem glob.
fn expand_resolved(entries: &[ResolvedFile]) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in entries {
        match entry {
            ResolvedFile::Input(p) | ResolvedFile::Output(p) => {
                files.push(p.clone());
            }
            ResolvedFile::InputGlob(pattern) => {
                if let Ok(paths) = glob::glob(pattern) {
                    for path in paths.flatten() {
                        if path.is_file() {
                            files.push(path);
                        }
                    }
                }
            }
        }
    }
    files
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
    store: &dyn StateStore,
    output: &Output,
    target: Option<&str>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = match target {
        Some(t) => dag.target_order(t)?,
        None => dag.topo_order()?,
    };

    let mut scope = base.scope.clone();
    let mut plans = Vec::new();
    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut hash_cache = HashCache::new();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let (provider_state, stored_outputs, stored_content_hash) = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => (None, Map::new(), String::new()),
        };

        // Resolve files
        let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
            block: name.clone(),
            phase: "resolve",
            source: e,
        })?;

        // Hash inputs + existing outputs + parent states to detect changes
        let all_files = expand_resolved(&resolved);
        let has_dirty_dep = dag.content_deps(name).iter().any(|d| dirty.contains(d));
        let current_content_hash = compute_content_hash(&all_files, dag, name, store, &mut hash_cache);
        let inputs_changed = has_dirty_dep || current_content_hash != stored_content_hash;

        let mut result = node
            .resource
            .plan(&inputs, provider_state.as_ref())
            .map_err(|e| EngineError::Provider {
                block: name.clone(),
                phase: "plan",
                source: e,
            })?;

        if result.action == PlanAction::None && inputs_changed && provider_state.is_some() {
            result.action = PlanAction::Update;
        }

        if result.action != PlanAction::None {
            dirty.insert(name.clone());
        }

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

        // Use stored outputs so downstream blocks can reference them
        scope.set(name, Value::Map(stored_outputs));

        plans.push(BlockPlan {
            name: name.clone(),
            plan: result,
        });
    }

    Ok(plans)
}

/// Apply all blocks in the DAG (or a target subset).
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
    apply_order(dag, base, store, output, &order)
}

/// Apply only test blocks and their transitive dependencies.
pub fn test(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = dag.test_order()?;
    apply_order(dag, base, store, output, &order)
}

/// Apply blocks in the given order.
fn apply_order(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    order: &[String],
) -> Result<Vec<BlockPlan>, EngineError> {
    let mut scope = base.scope.clone();
    let mut results = Vec::new();
    let mut hash_cache = HashCache::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let (provider_state, stored_outputs, stored_content_hash) = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => (None, Map::new(), String::new()),
        };

        // Resolve files and compute combined hash
        let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
            block: name.clone(),
            phase: "resolve",
            source: e,
        })?;
        let all_files = expand_resolved(&resolved);
        let current_content_hash = compute_content_hash(&all_files, dag, name, store, &mut hash_cache);
        let inputs_changed = current_content_hash != stored_content_hash;

        // Never skip previously failed test blocks
        let previously_failed = node.resource.kind() == ResourceKind::Test
            && stored_outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

        let mut plan_result =
            node.resource
                .plan(&inputs, provider_state.as_ref())
                .map_err(|e| EngineError::Provider {
                    block: name.clone(),
                    phase: "plan",
                    source: e,
                })?;

        // Engine forces update if inputs changed or test previously failed
        if plan_result.action == PlanAction::None && (inputs_changed || previously_failed) && provider_state.is_some() {
            plan_result.action = PlanAction::Update;
        };

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
            scope.set(name, Value::Map(stored_outputs));
            results.push(BlockPlan {
                name: name.clone(),
                plan: plan_result,
            });
            continue;
        }

        writer.event(Event::Starting, &plan_result.description);

        let apply_result = node
            .resource
            .apply(&inputs, provider_state.as_ref(), &writer)
            .map_err(|e| {
                writer.event(Event::Failed, &e.to_string());
                EngineError::Provider {
                    block: name.clone(),
                    phase: "apply",
                    source: e,
                }
            })?;

        // Persist wrapped state (provider state + outputs + content hash).
        // Re-resolve after apply so output files are included in the hash.
        // Invalidate cache for output files since apply may have changed them.
        if let Some(provider_state) = &apply_result.state {
            let post_entries = node.resource.resolve(&inputs).unwrap_or_default();
            // Invalidate cache for output files since apply may have changed them
            for entry in &post_entries {
                if let ResolvedFile::Output(p) = entry {
                    hash_cache.remove(p);
                }
            }
            let post_files = expand_resolved(&post_entries);
            let content_hash = compute_content_hash(&post_files, dag, name, store, &mut hash_cache);
            let wrapped = WrappedState {
                state: provider_state.clone(),
                outputs: apply_result.outputs.clone(),
                content_hash,
            };
            store.save(name, &serde_json::to_value(&wrapped).unwrap())?;
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

        let Some(stored) = &node.prior_state else {
            writer.event(Event::Skipped, "no state");
            continue;
        };

        let (provider_state, _, _) = unwrap_state(stored);
        let Some(provider_state) = provider_state else {
            writer.event(Event::Skipped, "no state");
            continue;
        };

        writer.event(Event::Starting, "destroying");

        node.resource.destroy(&provider_state, &writer).map_err(|e| {
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
        let out = Output::new(&[]);
        let plans = plan(&mut dag, &base, &store, &out, None).unwrap();
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
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, None).unwrap();
        assert!(!store.list().unwrap().is_empty());

        // Reload with state, then destroy
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        destroy(&mut dag, &store, &out, None).unwrap();
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
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, None).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        destroy(&mut dag, &store, &out, None).unwrap();
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
        let out = Output::new(&[]);
        let results = apply(&mut dag, &base, &store, &out, Some("just_a")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "a");
        assert!(out_a.exists());
        assert!(!out_b.exists());
    }
}
