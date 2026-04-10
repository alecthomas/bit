use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::dag::{self, Dag, DagError};
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

/// File modification time as nanoseconds since UNIX epoch, stored as a string
/// because the value exceeds JSON's safe integer range (2^53).
type FileTimestamp = String;

/// Read a `SystemTime` into our string-of-nanos representation.
fn timestamp_from_system_time(t: SystemTime) -> FileTimestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    d.as_nanos().to_string()
}

/// Wrapped state persisted by the engine. Contains the provider's own state,
/// outputs, and a combined hash of all inputs (resolved files + parent states).
#[derive(serde::Serialize, serde::Deserialize)]
struct WrappedState {
    state: serde_json::Value,
    outputs: Map,
    content_hash: String,
    /// Modification timestamps of input/output files at last apply.
    /// Keyed by file path string for portable serialization.
    #[serde(default)]
    file_timestamps: HashMap<String, FileTimestamp>,
    /// Content hashes of parent blocks at last apply, keyed by block name.
    #[serde(default)]
    dep_hashes: HashMap<String, String>,
}

/// Extracted prior state fields returned by `unwrap_state`.
struct PriorState {
    provider_state: Option<serde_json::Value>,
    outputs: Map,
    content_hash: String,
    file_timestamps: HashMap<String, FileTimestamp>,
    dep_hashes: HashMap<String, String>,
}

/// Extract the provider state, outputs, and stored content hash from persisted state.
fn unwrap_state(stored: &serde_json::Value) -> PriorState {
    let wrapped: WrappedState =
        serde_json::from_value(stored.clone()).expect("corrupted state: not a valid WrappedState");
    PriorState {
        provider_state: Some(wrapped.state),
        outputs: wrapped.outputs,
        content_hash: wrapped.content_hash,
        file_timestamps: wrapped.file_timestamps,
        dep_hashes: wrapped.dep_hashes,
    }
}

fn default_prior() -> PriorState {
    PriorState {
        provider_state: None,
        outputs: Map::new(),
        content_hash: String::new(),
        file_timestamps: HashMap::new(),
        dep_hashes: HashMap::new(),
    }
}

/// Cache of file path -> content hash, shared across all blocks in a run.
type HashCache = HashMap<PathBuf, String>;

/// Cache of file path -> mtime, shared across all blocks in a run.
type MtimeCache = HashMap<PathBuf, FileTimestamp>;

/// Return the modification time of a file, using the shared cache.
fn cached_mtime(path: &std::path::Path, cache: &mut MtimeCache) -> Option<FileTimestamp> {
    if let Some(ts) = cache.get(path) {
        return Some(ts.clone());
    }
    let ts = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(timestamp_from_system_time)?;
    cache.insert(path.to_path_buf(), ts.clone());
    Some(ts)
}

/// Check whether all file timestamps and dep hashes match the stored values.
/// Returns `true` when the stored content hash can be reused (fast path).
fn timestamps_unchanged(
    files: &[PathBuf],
    stored_timestamps: &HashMap<String, FileTimestamp>,
    dag: &Dag,
    block_name: &str,
    store: &dyn StateStore,
    stored_dep_hashes: &HashMap<String, String>,
    mtime_cache: &mut MtimeCache,
) -> bool {
    // File set must be identical in size.
    if files.len() != stored_timestamps.len() {
        return false;
    }

    // Every file must exist in stored set with matching mtime.
    for file in files {
        let key = file.to_string_lossy();
        let Some(stored) = stored_timestamps.get(key.as_ref()) else {
            return false;
        };
        let Some(current) = cached_mtime(file, mtime_cache) else {
            return false;
        };
        if current != *stored {
            return false;
        }
    }

    // Parent dep content hashes must also be unchanged.
    let deps = dag.content_deps(block_name);
    if deps.len() != stored_dep_hashes.len() {
        return false;
    }
    for dep in &deps {
        let Some(stored_hash) = stored_dep_hashes.get(dep) else {
            return false;
        };
        let Ok(Some(parent_state)) = store.load(dep) else {
            return false;
        };
        let parent = unwrap_state(&parent_state);
        if parent.content_hash != *stored_hash {
            return false;
        }
    }

    true
}

/// Result of content hash computation, including metadata for fast-path caching.
struct ContentHashResult {
    hash: String,
    file_timestamps: HashMap<String, FileTimestamp>,
    dep_hashes: HashMap<String, String>,
}

/// Compute a combined hash of files and parent block states.
/// If stored timestamps indicate nothing changed, returns the stored hash (fast path).
fn compute_content_hash(
    files: &[PathBuf],
    dag: &Dag,
    block_name: &str,
    store: &dyn StateStore,
    cache: &mut HashCache,
    mtime_cache: &mut MtimeCache,
    prior: &PriorState,
) -> ContentHashResult {
    // Fast path: if all timestamps and dep hashes match, reuse stored hash.
    if !prior.content_hash.is_empty()
        && timestamps_unchanged(
            files,
            &prior.file_timestamps,
            dag,
            block_name,
            store,
            &prior.dep_hashes,
            mtime_cache,
        )
    {
        return ContentHashResult {
            hash: prior.content_hash.clone(),
            file_timestamps: prior.file_timestamps.clone(),
            dep_hashes: prior.dep_hashes.clone(),
        };
    }

    // Slow path: full content hash.
    let mut hasher = Sha256::new();
    let mut new_timestamps = HashMap::with_capacity(files.len());

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
        if let Some(ts) = cached_mtime(file, mtime_cache) {
            new_timestamps.insert(file.to_string_lossy().into_owned(), ts);
        }
    }

    // Hash parent block states (content-coupled deps only)
    let mut deps = dag.content_deps(block_name);
    deps.sort();
    let mut new_dep_hashes = HashMap::with_capacity(deps.len());
    for dep in &deps {
        if let Ok(Some(state)) = store.load(dep) {
            let parent = unwrap_state(&state);
            new_dep_hashes.insert(dep.clone(), parent.content_hash.clone());
            hasher.update(dep.as_bytes());
            hasher.update(state.to_string().as_bytes());
        }
    }

    ContentHashResult {
        hash: format!("sha256:{:x}", hasher.finalize()),
        file_timestamps: new_timestamps,
        dep_hashes: new_dep_hashes,
    }
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
    let mut mtime_cache = MtimeCache::new();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let prior = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => default_prior(),
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
        let hash_result = compute_content_hash(
            &all_files, dag, name, store, &mut hash_cache, &mut mtime_cache, &prior,
        );
        let inputs_changed =
            has_dirty_dep || hash_result.hash != prior.content_hash;

        let mut result = node
            .resource
            .plan(&inputs, prior.provider_state.as_ref())
            .map_err(|e| EngineError::Provider {
                block: name.clone(),
                phase: "plan",
                source: e,
            })?;

        if result.action == PlanAction::None && inputs_changed && prior.provider_state.is_some() {
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
        scope.set(name, Value::Map(prior.outputs));

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
    let mut mtime_cache = MtimeCache::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;

        let prior = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => default_prior(),
        };

        // Resolve files and compute combined hash
        let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
            block: name.clone(),
            phase: "resolve",
            source: e,
        })?;
        let all_files = expand_resolved(&resolved);
        let hash_result = compute_content_hash(
            &all_files, dag, name, store, &mut hash_cache, &mut mtime_cache, &prior,
        );
        let inputs_changed = hash_result.hash != prior.content_hash;

        // Never skip previously failed test blocks
        let previously_failed = node.resource.kind() == ResourceKind::Test
            && prior.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

        let mut plan_result =
            node.resource
                .plan(&inputs, prior.provider_state.as_ref())
                .map_err(|e| EngineError::Provider {
                    block: name.clone(),
                    phase: "plan",
                    source: e,
                })?;

        // Engine forces update if inputs changed or test previously failed
        if plan_result.action == PlanAction::None
            && (inputs_changed || previously_failed)
            && prior.provider_state.is_some()
        {
            plan_result.action = PlanAction::Update;
        }

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
            scope.set(name, Value::Map(prior.outputs));
            results.push(BlockPlan {
                name: name.clone(),
                plan: plan_result,
            });
            continue;
        }

        writer.event(Event::Starting, &plan_result.description);

        let apply_result = node
            .resource
            .apply(&inputs, prior.provider_state.as_ref(), &writer)
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
        if let Some(new_state) = &apply_result.state {
            let post_entries = node.resource.resolve(&inputs).unwrap_or_default();
            for entry in &post_entries {
                if let ResolvedFile::Output(p) = entry {
                    hash_cache.remove(p);
                    mtime_cache.remove(p);
                }
            }
            let post_files = expand_resolved(&post_entries);
            // Force full hash on post-apply (no fast path — files just changed).
            let post_prior = default_prior();
            let post_hash = compute_content_hash(
                &post_files, dag, name, store, &mut hash_cache, &mut mtime_cache, &post_prior,
            );
            let wrapped = WrappedState {
                state: new_state.clone(),
                outputs: apply_result.outputs.clone(),
                content_hash: post_hash.hash,
                file_timestamps: post_hash.file_timestamps,
                dep_hashes: post_hash.dep_hashes,
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

        let prior = unwrap_state(stored);
        let Some(provider_state) = prior.provider_state else {
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

/// Dump evaluated inputs and stored outputs for all blocks (or a target subset).
pub fn dump(
    dag: &mut Dag,
    base: &BaseScope,
    target: Option<&str>,
) -> Result<(), EngineError> {
    let order = match target {
        Some(t) => dag.target_order(t)?,
        None => dag.topo_order()?,
    };

    let mut scope = base.scope.clone();

    for (i, name) in order.iter().enumerate() {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;

        // Evaluate inputs, but replace depends_on/after with block names
        let mut inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            block: name.clone(),
            source: e,
        })?;
        let depends_on = dag::collect_depends_on(&node.fields);
        if !depends_on.is_empty() {
            inputs.insert(
                "depends_on".into(),
                Value::List(depends_on.into_iter().map(Value::Str).collect()),
            );
        }
        let after = dag::collect_after(&node.fields);
        if !after.is_empty() {
            inputs.insert(
                "after".into(),
                Value::List(after.into_iter().map(Value::Str).collect()),
            );
        }

        let prior = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => default_prior(),
        };

        // Populate scope with stored outputs for downstream refs
        scope.set(name, Value::Map(prior.outputs.clone()));

        if i > 0 {
            println!();
        }
        println!("{name}:");
        if !inputs.is_empty() {
            println!("  inputs:");
            let mut keys: Vec<&String> = inputs.keys().collect();
            keys.sort();
            for key in keys {
                print_value(key, &inputs[key], 4);
            }
        }
        if !prior.outputs.is_empty() {
            println!("  outputs:");
            let mut keys: Vec<&String> = prior.outputs.keys().collect();
            keys.sort();
            for key in keys {
                print_value(key, &prior.outputs[key], 4);
            }
        }
    }

    Ok(())
}

fn print_value(key: &str, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Map(map) => {
            println!("{pad}{key}:");
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                print_value(k, &map[k], indent + 2);
            }
        }
        Value::List(items) => {
            println!("{pad}{key}:");
            for item in items {
                println!("{pad}  - {item}");
            }
        }
        _ => println!("{pad}{key}: {value}"),
    }
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

    #[test]
    fn timestamp_fast_path_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();

        // First apply creates the block
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, None).unwrap();

        // Verify persisted state has timestamps
        let stored = store.load("build").unwrap().unwrap();
        let wrapped: WrappedState = serde_json::from_value(stored).unwrap();
        assert!(!wrapped.content_hash.is_empty());
        assert!(!wrapped.file_timestamps.is_empty());

        // Second apply should be a no-op (timestamp fast path)
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        let results = apply(&mut dag, &base, &store, &out, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::None);
    }

    #[test]
    fn timestamp_detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let input_file = dir.path().join("src.txt");
        let output_file = dir.path().join("out.txt");
        std::fs::write(&input_file, "v1").unwrap();

        let input = format!(
            "build = exec {{\n  command = \"cp {} {}\"\n  output = \"{}\"\n  inputs = [\"{}\"]\n}}\n",
            input_file.display(),
            output_file.display(),
            output_file.display(),
            input_file.display(),
        );
        let module = parser::parse(&input).unwrap();
        let store = MemoryStore::new();
        let out = Output::new(&[]);

        // First apply
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        apply(&mut dag, &base, &store, &out, None).unwrap();

        // Modify input file (touch with new content to change both mtime and hash)
        std::fs::write(&input_file, "v2").unwrap();

        // Second apply should detect the change
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(), &store).unwrap();
        let results = apply(&mut dag, &base, &store, &out, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::Update);
    }
}
