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
use crate::value::{Map, Type, Value};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("{pos}: eval error in block '{block}': {source}")]
    Eval {
        pos: crate::ast::Pos,
        block: String,
        source: EvalError,
    },
    #[error("{pos}: block '{block}' {phase} failed: {source}")]
    Provider {
        pos: crate::ast::Pos,
        block: String,
        phase: &'static str,
        source: BoxError,
    },
    #[error("{0}")]
    State(#[from] StateError),
    #[error("{pos}: protected block '{block}' cannot be {action}")]
    Protected {
        pos: crate::ast::Pos,
        block: String,
        action: &'static str,
    },
    #[error("{pos}: test block '{block}' failed")]
    TestFailed { pos: crate::ast::Pos, block: String },
}

/// Result of planning a single block.
pub struct BlockPlan {
    pub name: String,
    pub plan: PlanResult,
}

/// File modification time as nanoseconds since UNIX epoch, stored as a string
/// because the value exceeds JSON's safe integer range (2^53).
type FileTimestamp = String;

/// Truncate a hash string for compact debug output. Strips the `sha256:`
/// algorithm prefix (all engine hashes share it) then takes the first 8 hex
/// chars. Returns `"<none>"` for an empty string so "no prior hash" is
/// distinguishable from a real hash.
fn short_hash(h: &str) -> &str {
    if h.is_empty() {
        return "<none>";
    }
    let hex = h.strip_prefix("sha256:").unwrap_or(h);
    if hex.len() > 8 { &hex[..8] } else { hex }
}

/// Read a `SystemTime` into our string-of-nanos representation.
fn timestamp_from_system_time(t: SystemTime) -> FileTimestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    d.as_nanos().to_string()
}

/// Wrapped state persisted by the engine. Contains the provider's own state,
/// outputs, and the combined hash covering resolved files, parent states, and
/// evaluated input fields.
#[derive(serde::Serialize, serde::Deserialize)]
struct WrappedState {
    state: serde_json::Value,
    outputs: Map,
    content_hash: String,
    /// Hash of the block's evaluated input fields at last apply. Used as a
    /// fast-path signal: if `input_hash` matches the current inputs, the
    /// file+dep portion of `content_hash` can be reused from the prior run
    /// without re-reading file contents.
    #[serde(default)]
    input_hash: String,
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
    input_hash: String,
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
        input_hash: wrapped.input_hash,
        file_timestamps: wrapped.file_timestamps,
        dep_hashes: wrapped.dep_hashes,
    }
}

/// Resolve a block's prior state from its persisted representation, emitting
/// a debug line describing whether state was restored.
fn restore_prior(writer: &crate::output::BlockWriter, prior_state: Option<&serde_json::Value>) -> PriorState {
    match prior_state {
        Some(s) => {
            let prior = unwrap_state(s);
            crate::debug!(
                writer,
                "restored state from store (hash={})",
                short_hash(&prior.content_hash)
            );
            prior
        }
        None => {
            crate::debug!(writer, "no prior state in store");
            default_prior()
        }
    }
}

fn default_prior() -> PriorState {
    PriorState {
        provider_state: None,
        outputs: Map::new(),
        content_hash: String::new(),
        input_hash: String::new(),
        file_timestamps: HashMap::new(),
        dep_hashes: HashMap::new(),
    }
}

/// Hash the block's evaluated input fields in canonical (key-sorted) JSON form.
/// Routing via `serde_json::to_value` first forces the HashMap-backed `Map`
/// through serde_json's BTreeMap-backed object type, producing deterministic
/// key ordering regardless of HashMap iteration order.
fn hash_inputs(inputs: &Map) -> String {
    let canonical = serde_json::to_value(inputs)
        .and_then(|v| serde_json::to_string(&v))
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Cache of file path -> content hash, shared across all blocks in a run.
type HashCache = HashMap<PathBuf, String>;

/// Cache of file path -> mtime, shared across all blocks in a run.
type MtimeCache = HashMap<PathBuf, FileTimestamp>;

/// Cache of glob pattern -> expanded file paths, shared across all blocks in a run.
type GlobCache = HashMap<String, Vec<PathBuf>>;

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
    input_hash: String,
    file_timestamps: HashMap<String, FileTimestamp>,
    dep_hashes: HashMap<String, String>,
}

/// Describe why a block's content hash changed compared to its prior state.
fn relative_display(path: &std::path::Path, cwd: &std::path::Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().into_owned()
}

#[allow(clippy::too_many_arguments)]
fn change_reason(
    resolved: &[ResolvedFile],
    prior: &PriorState,
    dag: &Dag,
    block_name: &str,
    dirty_deps: &std::collections::HashSet<String>,
    input_hash: &str,
    mtime_cache: &mut MtimeCache,
    glob_cache: &mut GlobCache,
) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Check for dirty dependency blocks
    let mut dirty: Vec<_> = dag
        .content_deps(block_name)
        .into_iter()
        .filter(|d| dirty_deps.contains(d))
        .collect();
    dirty.dedup();
    if !dirty.is_empty() {
        let quoted: Vec<_> = dirty.iter().map(|d| format!("'{d}'")).collect();
        return Some(quoted.join(", ") + " changed");
    }

    // Check for changed files, classifying as input vs output
    let mut changed_inputs = Vec::new();
    let mut changed_outputs = Vec::new();
    for entry in resolved {
        let (path, is_output) = match entry {
            ResolvedFile::Input(p) => (p, false),
            ResolvedFile::Output(p) => (p, true),
            ResolvedFile::InputGlob(pattern) => {
                let expanded = glob_cache.entry(pattern.clone()).or_insert_with(|| {
                    let mut result = Vec::new();
                    if let Ok(paths) = glob::glob(pattern) {
                        for path in paths.flatten() {
                            if path.is_file() {
                                result.push(path);
                            }
                        }
                    }
                    result
                });
                for p in expanded {
                    let key = p.to_string_lossy();
                    let stored = prior.file_timestamps.get(key.as_ref());
                    let current = cached_mtime(p, mtime_cache);
                    if stored.map(String::as_str) != current.as_deref() {
                        changed_inputs.push(relative_display(p, &cwd));
                    }
                }
                continue;
            }
        };
        let key = path.to_string_lossy();
        let stored = prior.file_timestamps.get(key.as_ref());
        let current = cached_mtime(path, mtime_cache);
        if stored.map(String::as_str) != current.as_deref() {
            let display = relative_display(path, &cwd);
            if is_output {
                changed_outputs.push(display);
            } else {
                changed_inputs.push(display);
            }
        }
    }

    if !changed_inputs.is_empty() {
        let count = changed_inputs.len();
        if count <= 3 {
            let quoted: Vec<_> = changed_inputs.iter().map(|f| format!("'{f}'")).collect();
            return Some(quoted.join(", ") + " changed");
        }
        return Some(format!("{count} files changed"));
    }
    if !changed_outputs.is_empty() {
        let quoted: Vec<_> = changed_outputs.iter().map(|p| format!("'{p}'")).collect();
        return Some(format!("output {} modified externally", quoted.join(", ")));
    }

    // New files or removed files
    if prior.file_timestamps.len() != resolved.len() {
        return Some("file set changed".into());
    }

    // Fall back to inputs: if nothing above matched but the stored input_hash
    // differs, a field on the block itself must have changed (e.g. a config
    // string like `ports` or `image`).
    if !prior.input_hash.is_empty() && prior.input_hash != input_hash {
        return Some("inputs changed".into());
    }

    None
}

/// Compute a combined hash of files and parent block states.
/// If stored timestamps indicate nothing changed, returns the stored hash (fast path).
#[allow(clippy::too_many_arguments)]
fn compute_content_hash(
    files: &[PathBuf],
    dag: &Dag,
    block_name: &str,
    store: &dyn StateStore,
    cache: &mut HashCache,
    mtime_cache: &mut MtimeCache,
    input_hash: &str,
    prior: &PriorState,
) -> ContentHashResult {
    // Fast path: if inputs, file timestamps, and dep hashes all match the
    // prior run, reuse the stored content hash without re-reading files.
    if !prior.content_hash.is_empty()
        && prior.input_hash == input_hash
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
            input_hash: input_hash.to_owned(),
            file_timestamps: prior.file_timestamps.clone(),
            dep_hashes: prior.dep_hashes.clone(),
        };
    }

    // Slow path: full content hash.
    let mut hasher = Sha256::new();
    let mut new_timestamps = HashMap::with_capacity(files.len());

    // Fold in evaluated inputs first so changes to non-file fields (e.g. a
    // docker container's `ports` or `environment`) shift the hash.
    hasher.update(input_hash.as_bytes());

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
        input_hash: input_hash.to_owned(),
        file_timestamps: new_timestamps,
        dep_hashes: new_dep_hashes,
    }
}

/// Expand resolved file entries into concrete file paths.
/// InputGlob patterns are expanded via filesystem glob, with results cached.
fn expand_resolved(entries: &[ResolvedFile], glob_cache: &mut GlobCache) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in entries {
        match entry {
            ResolvedFile::Input(p) | ResolvedFile::Output(p) => {
                files.push(p.clone());
            }
            ResolvedFile::InputGlob(pattern) => {
                let expanded = glob_cache.entry(pattern.clone()).or_insert_with(|| {
                    let mut result = Vec::new();
                    if let Ok(paths) = glob::glob(pattern) {
                        for path in paths.flatten() {
                            if path.is_file() {
                                result.push(path);
                            }
                        }
                    }
                    result
                });
                files.extend(expanded.iter().cloned());
            }
        }
    }
    files
}

fn plan_action_to_event(action: &PlanAction) -> Event {
    match action {
        PlanAction::Create => Event::Create,
        PlanAction::Update => Event::Update,
        PlanAction::Destroy => Event::Destroy,
        PlanAction::None => Event::NoChange,
    }
}

/// Validate that active blocks don't reference missing (unresolved) params.
fn validate_active_params(dag: &Dag, order: &[String], base: &BaseScope) -> Result<(), EngineError> {
    if base.missing_params.is_empty() {
        return Ok(());
    }
    for name in order {
        let Some(node) = dag.get_node(name) else {
            continue;
        };
        for r in dag::collect_all_refs(&node.fields) {
            if base.missing_params.contains(&r) {
                return Err(EngineError::Eval {
                    pos: node.pos.clone(),
                    block: name.clone(),
                    source: crate::expr::EvalError::UndefinedVar(format!(
                        "missing required param '{r}' (use -P {r}=VALUE)"
                    )),
                });
            }
        }
    }
    Ok(())
}

/// Resolve the block execution order for a given target.
/// - `None` → use `default` target if defined, else all blocks
/// - empty → default target if defined, else all blocks
/// - `["..."]` → all blocks
/// - one or more names → union of their target/block orders
pub fn resolve_order(dag: &Dag, targets: &[String]) -> Result<Vec<String>, EngineError> {
    if targets.is_empty() {
        if dag.targets().contains_key("default") {
            Ok(dag.target_order("default")?)
        } else {
            Ok(dag.topo_order()?)
        }
    } else if targets.len() == 1 && targets[0] == "..." {
        Ok(dag.topo_order()?)
    } else {
        let mut needed = std::collections::HashSet::new();
        for t in targets {
            for name in dag.target_order(t)? {
                needed.insert(name);
            }
        }
        let all = dag.topo_order()?;
        Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
    }
}

/// Plan all blocks in the DAG (or a target subset), returning what would change.
pub fn plan(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    targets: &[String],
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    validate_active_params(dag, &order, base)?;
    crate::debug!(output, "plan: {} block(s) in order: {}", order.len(), order.join(", "));

    let mut scope = base.scope.clone();
    let mut plans = Vec::new();
    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut hash_cache = HashCache::new();
    let mut mtime_cache = MtimeCache::new();
    let mut glob_cache = GlobCache::new();

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer_indented(name, dag.depth(name));

        let inputs = eval_fields_lenient(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            block: name.clone(),
            source: e,
        })?;

        let prior = restore_prior(&writer, node.prior_state.as_ref());

        // Resolve files
        let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
            pos: node.pos.clone(),
            block: name.clone(),
            phase: "resolve",
            source: e,
        })?;

        // Hash inputs + existing outputs + parent states to detect changes
        let all_files = expand_resolved(&resolved, &mut glob_cache);
        let has_dirty_dep = dag.content_deps(name).iter().any(|d| dirty.contains(d));
        let input_hash = hash_inputs(&inputs);
        let hash_result = compute_content_hash(
            &all_files,
            dag,
            name,
            store,
            &mut hash_cache,
            &mut mtime_cache,
            &input_hash,
            &prior,
        );
        let inputs_changed = has_dirty_dep || hash_result.hash != prior.content_hash;
        crate::debug!(
            writer,
            "resolved {} file(s), hash={} prior_hash={} dirty_dep={} inputs_changed={}",
            all_files.len(),
            short_hash(&hash_result.hash),
            short_hash(&prior.content_hash),
            has_dirty_dep,
            inputs_changed,
        );

        let mut result = node
            .resource
            .plan(&inputs, prior.provider_state.as_ref())
            .map_err(|e| EngineError::Provider {
                pos: node.pos.clone(),
                block: name.clone(),
                phase: "plan",
                source: e,
            })?;

        if result.action == PlanAction::None && inputs_changed && prior.provider_state.is_some() {
            result.action = PlanAction::Update;
            if result.reason.is_none() {
                result.reason = change_reason(
                    &resolved,
                    &prior,
                    dag,
                    name,
                    &dirty,
                    &input_hash,
                    &mut mtime_cache,
                    &mut glob_cache,
                );
            }
        }

        if result.action != PlanAction::None {
            dirty.insert(name.clone());
        }

        if node.protected && result.action == PlanAction::Destroy {
            return Err(EngineError::Protected {
                pos: node.pos.clone(),
                block: name.clone(),
                action: "destroyed",
            });
        }

        crate::debug!(writer, "planned action: {:?}", result.action);
        let event = plan_action_to_event(&result.action);
        emit_event(&writer, event, &result.description, result.reason.as_deref());

        // Use stored outputs so downstream blocks can reference them
        scope.set(name, Value::strct(prior.outputs));

        plans.push(BlockPlan {
            name: name.clone(),
            plan: result,
        });
    }

    Ok(plans)
}

/// Apply all blocks in the DAG (or a target subset).
/// With no targets: runs the `default` target if defined, else all blocks.
/// With `["..."]`: runs all blocks.
pub fn apply(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    targets: &[String],
    jobs: usize,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    validate_active_params(dag, &order, base)?;
    crate::debug!(
        output,
        "apply: {} block(s), jobs={}, order: {}",
        order.len(),
        jobs,
        order.join(", ")
    );
    if jobs <= 1 {
        apply_order(dag, base, store, output, &order)
    } else {
        apply_order_parallel(dag, base, store, output, &order, jobs)
    }
}

/// Apply only test blocks and their transitive dependencies.
pub fn test(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    jobs: usize,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = dag.test_order()?;
    crate::debug!(
        output,
        "test: {} block(s), jobs={}, order: {}",
        order.len(),
        jobs,
        order.join(", ")
    );
    if jobs <= 1 {
        apply_order(dag, base, store, output, &order)
    } else {
        apply_order_parallel(dag, base, store, output, &order, jobs)
    }
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
    let mut glob_cache = GlobCache::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        let inputs = eval_fields(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            block: name.clone(),
            source: e,
        })?;

        let prior = restore_prior(&writer, node.prior_state.as_ref());

        // Resolve files and compute combined hash
        let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
            pos: node.pos.clone(),
            block: name.clone(),
            phase: "resolve",
            source: e,
        })?;
        let all_files = expand_resolved(&resolved, &mut glob_cache);
        let input_hash = hash_inputs(&inputs);
        let hash_result = compute_content_hash(
            &all_files,
            dag,
            name,
            store,
            &mut hash_cache,
            &mut mtime_cache,
            &input_hash,
            &prior,
        );
        let inputs_changed = hash_result.hash != prior.content_hash;

        // Never skip previously failed test blocks
        let previously_failed = node.resource.kind() == ResourceKind::Test
            && prior.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

        let mut plan_result = node
            .resource
            .plan(&inputs, prior.provider_state.as_ref())
            .map_err(|e| EngineError::Provider {
                pos: node.pos.clone(),
                block: name.clone(),
                phase: "plan",
                source: e,
            })?;

        // Engine forces update if inputs changed or test previously failed.
        if plan_result.action == PlanAction::None
            && (inputs_changed || previously_failed)
            && prior.provider_state.is_some()
        {
            plan_result.action = PlanAction::Update;
            if plan_result.reason.is_none() {
                plan_result.reason = if previously_failed {
                    Some("previously failed".into())
                } else {
                    change_reason(
                        &resolved,
                        &prior,
                        dag,
                        name,
                        &std::collections::HashSet::new(),
                        &input_hash,
                        &mut mtime_cache,
                        &mut glob_cache,
                    )
                };
            }
        }

        if node.protected && plan_result.action == PlanAction::Destroy {
            return Err(EngineError::Protected {
                pos: node.pos.clone(),
                block: name.clone(),
                action: "destroyed",
            });
        }

        if plan_result.action == PlanAction::None {
            writer.event(Event::Skipped, "no changes");
            scope.set(name, Value::strct(prior.outputs));
            results.push(BlockPlan {
                name: name.clone(),
                plan: plan_result,
            });
            continue;
        }

        emit_event(
            &writer,
            Event::Starting,
            &plan_result.description,
            plan_result.reason.as_deref(),
        );

        let apply_result = node
            .resource
            .apply(&inputs, prior.provider_state.as_ref(), &writer)
            .map_err(|e| {
                writer.event(Event::Failed, &e.to_string());
                EngineError::Provider {
                    pos: node.pos.clone(),
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
            let post_files = expand_resolved(&post_entries, &mut glob_cache);
            // Force full hash on post-apply (no fast path — files just changed).
            let post_prior = default_prior();
            let post_hash = compute_content_hash(
                &post_files,
                dag,
                name,
                store,
                &mut hash_cache,
                &mut mtime_cache,
                &input_hash,
                &post_prior,
            );
            let wrapped = WrappedState {
                state: new_state.clone(),
                outputs: apply_result.outputs.clone(),
                content_hash: post_hash.hash,
                input_hash: post_hash.input_hash,
                file_timestamps: post_hash.file_timestamps,
                dep_hashes: post_hash.dep_hashes,
            };
            store.save(name, &serde_json::to_value(&wrapped).unwrap())?;
            crate::debug!(writer, "saved state (hash={})", short_hash(&wrapped.content_hash));
        }

        // Check test blocks
        if node.resource.kind() == ResourceKind::Test
            && let Some(passed) = apply_result.outputs.get("passed").and_then(|v| v.as_bool())
            && !passed
        {
            writer.event(Event::Failed, "tests failed");
            return Err(EngineError::TestFailed {
                pos: node.pos.clone(),
                block: name.clone(),
            });
        }

        writer.event(Event::Ok, "");

        // Inject outputs into scope for downstream blocks
        scope.set(name, Value::strct(apply_result.outputs));

        results.push(BlockPlan {
            name: name.clone(),
            plan: plan_result,
        });
    }

    Ok(results)
}

/// Result sent back from a worker thread after executing a block.
struct BlockResult {
    pos: crate::ast::Pos,
    name: String,
    plan: PlanResult,
    outputs: Map,
    /// The wrapped state to persist, if the block was applied.
    wrapped_state: Option<serde_json::Value>,
    /// Whether this was a failed test (passed == false).
    test_failed: bool,
}

/// Apply blocks in parallel using a ready-queue scheduler.
fn apply_order_parallel(
    dag: &Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    output: &Output,
    order: &[String],
    jobs: usize,
) -> Result<Vec<BlockPlan>, EngineError> {
    use std::collections::{HashSet, VecDeque};
    use std::sync::mpsc;

    let order_set: HashSet<&str> = order.iter().map(|s| s.as_str()).collect();

    // Compute initial dep counts (only counting deps within the execution order)
    let mut remaining_deps: HashMap<String, usize> = HashMap::new();
    for name in order {
        let count = dag.deps(name).iter().filter(|d| order_set.contains(d.as_str())).count();
        remaining_deps.insert(name.clone(), count);
    }

    let mut ready: VecDeque<String> = VecDeque::new();
    for name in order {
        if remaining_deps[name] == 0 {
            ready.push_back(name.clone());
        }
    }

    let mut scope = base.scope.clone();
    let mut results: Vec<BlockPlan> = Vec::new();
    let mut completed = 0;
    let total = order.len();

    std::thread::scope(|s| {
        let (result_tx, result_rx) = mpsc::channel::<Result<BlockResult, EngineError>>();
        let mut in_flight = 0;
        let mut failed: Option<EngineError> = None;

        loop {
            // Dispatch ready blocks up to the job limit
            while in_flight < jobs && !ready.is_empty() && failed.is_none() {
                let name = ready.pop_front().expect("ready is non-empty");
                let node = dag.get_node(&name).expect("block in order");
                let writer = output.writer(&name);
                let scope_snapshot = scope.clone();
                let tx = result_tx.clone();

                s.spawn(move || {
                    let result = execute_block(&name, node, dag, store, &scope_snapshot, &writer);
                    let _ = tx.send(result);
                });
                in_flight += 1;
            }

            if in_flight == 0 {
                break;
            }

            // Wait for a result
            let result = result_rx.recv().expect("channel open");
            in_flight -= 1;

            match result {
                Ok(block_result) => {
                    let name = &block_result.name;

                    // Persist state
                    if let Some(state) = &block_result.wrapped_state {
                        if let Err(e) = store.save(name, state) {
                            failed = Some(e.into());
                            continue;
                        }
                        crate::debug!(output.writer(name), "saved state");
                    }

                    // Merge outputs into scope
                    scope.set(name, Value::strct(block_result.outputs.clone()));

                    // Check test failure
                    if block_result.test_failed {
                        failed = Some(EngineError::TestFailed {
                            pos: block_result.pos.clone(),
                            block: name.clone(),
                        });
                    }

                    results.push(BlockPlan {
                        name: name.clone(),
                        plan: block_result.plan,
                    });
                    completed += 1;

                    // Unblock dependents
                    if failed.is_none() {
                        for dep in dag.dependents(name) {
                            if let Some(count) = remaining_deps.get_mut(&dep) {
                                *count -= 1;
                                if *count == 0 {
                                    ready.push_back(dep);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    if failed.is_none() {
                        failed = Some(e);
                    }
                    completed += 1;
                }
            }

            // All done or draining after failure
            if completed >= total {
                break;
            }
        }

        match failed {
            Some(e) => Err(e),
            None => Ok(results),
        }
    })
}

/// Execute a single block: evaluate fields, plan, apply if needed, compute post-apply hash.
fn execute_block(
    name: &str,
    node: &crate::dag::DagNode,
    dag: &Dag,
    store: &dyn StateStore,
    scope: &Scope,
    writer: &crate::output::BlockWriter,
) -> Result<BlockResult, EngineError> {
    let inputs = eval_fields(&node.fields, scope).map_err(|e| EngineError::Eval {
        pos: node.pos.clone(),
        block: name.to_owned(),
        source: e,
    })?;

    let prior = restore_prior(writer, node.prior_state.as_ref());

    let resolved = node.resource.resolve(&inputs).map_err(|e| EngineError::Provider {
        pos: node.pos.clone(),
        block: name.to_owned(),
        phase: "resolve",
        source: e,
    })?;
    let mut glob_cache = GlobCache::new();
    let all_files = expand_resolved(&resolved, &mut glob_cache);
    let mut hash_cache = HashCache::new();
    let mut mtime_cache = MtimeCache::new();
    let input_hash = hash_inputs(&inputs);
    let hash_result = compute_content_hash(
        &all_files,
        dag,
        name,
        store,
        &mut hash_cache,
        &mut mtime_cache,
        &input_hash,
        &prior,
    );
    let inputs_changed = hash_result.hash != prior.content_hash;
    crate::debug!(
        writer,
        "resolved {} file(s), hash={} prior_hash={} inputs_changed={}",
        all_files.len(),
        short_hash(&hash_result.hash),
        short_hash(&prior.content_hash),
        inputs_changed,
    );

    let previously_failed = node.resource.kind() == ResourceKind::Test
        && prior.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

    let mut plan_result = node
        .resource
        .plan(&inputs, prior.provider_state.as_ref())
        .map_err(|e| EngineError::Provider {
            pos: node.pos.clone(),
            block: name.to_owned(),
            phase: "plan",
            source: e,
        })?;

    if plan_result.action == PlanAction::None && (inputs_changed || previously_failed) && prior.provider_state.is_some()
    {
        plan_result.action = PlanAction::Update;
        if plan_result.reason.is_none() {
            plan_result.reason = if previously_failed {
                Some("previously failed".into())
            } else {
                change_reason(
                    &resolved,
                    &prior,
                    dag,
                    name,
                    &std::collections::HashSet::new(),
                    &input_hash,
                    &mut mtime_cache,
                    &mut glob_cache,
                )
            };
        }
    }

    if node.protected && plan_result.action == PlanAction::Destroy {
        return Err(EngineError::Protected {
            pos: node.pos.clone(),
            block: name.to_owned(),
            action: "destroyed",
        });
    }

    crate::debug!(
        writer,
        "planned action: {:?} previously_failed={}",
        plan_result.action,
        previously_failed
    );

    if plan_result.action == PlanAction::None {
        writer.event(Event::Skipped, "no changes");
        return Ok(BlockResult {
            pos: node.pos.clone(),
            name: name.to_owned(),
            plan: plan_result,
            outputs: prior.outputs,
            wrapped_state: None,
            test_failed: false,
        });
    }

    emit_event(
        writer,
        Event::Starting,
        &plan_result.description,
        plan_result.reason.as_deref(),
    );

    let apply_result = node
        .resource
        .apply(&inputs, prior.provider_state.as_ref(), writer)
        .map_err(|e| {
            writer.event(Event::Failed, &e.to_string());
            EngineError::Provider {
                pos: node.pos.clone(),
                block: name.to_owned(),
                phase: "apply",
                source: e,
            }
        })?;

    // Compute post-apply hash
    let wrapped_state = if let Some(new_state) = &apply_result.state {
        let post_entries = node.resource.resolve(&inputs).unwrap_or_default();
        let post_files = expand_resolved(&post_entries, &mut glob_cache);
        let mut post_hash_cache = HashCache::new();
        let mut post_mtime_cache = MtimeCache::new();
        let post_prior = default_prior();
        let post_hash = compute_content_hash(
            &post_files,
            dag,
            name,
            store,
            &mut post_hash_cache,
            &mut post_mtime_cache,
            &input_hash,
            &post_prior,
        );
        crate::debug!(writer, "post-apply hash={}", short_hash(&post_hash.hash));
        let wrapped = WrappedState {
            state: new_state.clone(),
            outputs: apply_result.outputs.clone(),
            content_hash: post_hash.hash,
            input_hash: post_hash.input_hash,
            file_timestamps: post_hash.file_timestamps,
            dep_hashes: post_hash.dep_hashes,
        };
        Some(serde_json::to_value(&wrapped).expect("serialize wrapped state"))
    } else {
        crate::debug!(writer, "apply returned no state (stateless block)");
        None
    };

    let test_failed = node.resource.kind() == ResourceKind::Test
        && apply_result.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

    if test_failed {
        writer.event(Event::Failed, "tests failed");
    } else {
        writer.event(Event::Ok, "");
    }

    Ok(BlockResult {
        pos: node.pos.clone(),
        name: name.to_owned(),
        plan: plan_result,
        outputs: apply_result.outputs,
        wrapped_state,
        test_failed,
    })
}

/// Destroy blocks in reverse dependency order.
pub fn destroy(dag: &mut Dag, store: &dyn StateStore, output: &Output, targets: &[String]) -> Result<(), EngineError> {
    let mut order = resolve_order(dag, targets)?;
    order.reverse();
    crate::debug!(
        output,
        "destroy: {} block(s) in reverse order: {}",
        order.len(),
        order.join(", ")
    );

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        if node.protected {
            writer.event(Event::Protected, "protected");
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

        node.resource.destroy(&provider_state, &writer).map_err(|e| {
            writer.event(Event::Failed, &e.to_string());
            EngineError::Provider {
                pos: node.pos.clone(),
                block: name.clone(),
                phase: "destroy",
                source: e,
            }
        })?;

        store.remove(name)?;
        crate::debug!(writer, "removed state from store");
        writer.event(Event::Ok, "");
    }

    Ok(())
}

/// Dump evaluated inputs and stored outputs for all blocks (or a target subset).
pub fn dump(dag: &mut Dag, base: &BaseScope, targets: &[String]) -> Result<(), EngineError> {
    let order = resolve_order(dag, targets)?;

    let mut scope = base.scope.clone();

    for (i, name) in order.iter().enumerate() {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;

        // Evaluate inputs, but replace depends_on/after with block names
        let mut inputs = eval_fields_lenient(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            block: name.clone(),
            source: e,
        })?;
        let depends_on = dag::collect_depends_on(&node.fields);
        if !depends_on.is_empty() {
            inputs.insert(
                "depends_on".into(),
                Value::List(Type::String, depends_on.into_iter().map(Value::Str).collect()),
            );
        }
        let after = dag::collect_after(&node.fields);
        if !after.is_empty() {
            inputs.insert(
                "after".into(),
                Value::List(Type::String, after.into_iter().map(Value::Str).collect()),
            );
        }

        let prior = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => default_prior(),
        };

        // Populate scope with stored outputs for downstream refs
        scope.set(name, Value::strct(prior.outputs.clone()));

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
        Value::Map(_, map) | Value::Struct(_, map) => {
            println!("{pad}{key}:");
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                print_value(k, &map[k], indent + 2);
            }
        }
        Value::List(_, items) => {
            println!("{pad}{key}:");
            for item in items {
                println!("{pad}  - {item}");
            }
        }
        _ => println!("{pad}{key}: {value}"),
    }
}

/// Emit an event, appending a dimmed reason to the first line if present.
fn emit_event(writer: &crate::output::BlockWriter, event: Event, description: &str, reason: Option<&str>) {
    use yansi::Paint;
    match reason {
        Some(reason) => {
            let mut lines = description.lines();
            let first = lines.next().unwrap_or("");
            let styled_first = format!("{} {}", first.paint(event.color()), format!("({reason})").dim());
            let rest: Vec<_> = lines.map(|l| format!("{}", l.paint(event.color()))).collect();
            if rest.is_empty() {
                writer.event_raw(event, &styled_first);
            } else {
                let styled_rest: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
                let full = std::iter::once(styled_first.as_str())
                    .chain(styled_rest)
                    .collect::<Vec<_>>()
                    .join("\n");
                writer.event_raw(event, &full);
            }
        }
        None => writer.event(event, description),
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

fn eval_fields_lenient(fields: &[crate::ast::Field], scope: &Scope) -> Result<Map, EvalError> {
    let mut inputs = Map::new();
    for field in fields {
        let value = expr::eval_lenient(&field.value, scope)?;
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
        data: std::sync::RwLock<std::collections::HashMap<String, serde_json::Value>>,
    }

    impl MemoryStore {
        fn new() -> Self {
            Self {
                data: std::sync::RwLock::new(std::collections::HashMap::new()),
            }
        }
    }

    impl StateStore for MemoryStore {
        fn load(&self, block: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(self.data.read().unwrap().get(block).cloned())
        }
        fn save(&self, block: &str, state: &serde_json::Value) -> Result<(), StateError> {
            self.data.write().unwrap().insert(block.into(), state.clone());
            Ok(())
        }
        fn remove(&self, block: &str) -> Result<(), StateError> {
            self.data.write().unwrap().remove(block);
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(self.data.read().unwrap().keys().cloned().collect())
        }
    }

    fn load_and_apply(input: &str) -> Result<Vec<BlockPlan>, EngineError> {
        let module = parser::parse(input, "<test>").expect("parse failed");
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .expect("load failed");
        let output = Output::new(&[]);
        apply(&mut dag, &base, &store, &output, &[], 1)
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let out = Output::new(&[]);
        let plans = plan(&mut dag, &base, &store, &out, &[]).unwrap();
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        // Apply first
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, &[], 1).unwrap();
        assert!(!store.list().unwrap().is_empty());

        // Reload with state, then destroy
        let (mut dag, _base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        destroy(&mut dag, &store, &out, &[]).unwrap();
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, &[], 1).unwrap();

        let (mut dag, _base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        destroy(&mut dag, &store, &out, &[]).unwrap();
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let out = Output::new(&[]);
        let results = apply(&mut dag, &base, &store, &out, &["just_a".into()], 1).unwrap();
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        // First apply creates the block
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &out, &[], 1).unwrap();

        // Verify persisted state has timestamps
        let stored = store.load("build").unwrap().unwrap();
        let wrapped: WrappedState = serde_json::from_value(stored).unwrap();
        assert!(!wrapped.content_hash.is_empty());
        assert!(!wrapped.file_timestamps.is_empty());

        // Second apply should be a no-op (timestamp fast path)
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let results = apply(&mut dag, &base, &store, &out, &[], 1).unwrap();
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
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let out = Output::new(&[]);

        // First apply
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        apply(&mut dag, &base, &store, &out, &[], 1).unwrap();

        // Modify input file (touch with new content to change both mtime and hash)
        std::fs::write(&input_file, "v2").unwrap();

        // Second apply should detect the change
        let (mut dag, base) = loader::load(
            &module,
            &Map::new(),
            &test_registry(),
            &store,
            std::path::Path::new("."),
        )
        .unwrap();
        let results = apply(&mut dag, &base, &store, &out, &[], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::Update);
    }

    #[test]
    fn test_relative_display() {
        use std::path::Path;

        let cwd = Path::new("/home/user/project");
        assert_eq!(
            relative_display(Path::new("/home/user/project/src/main.rs"), cwd),
            "src/main.rs"
        );
        assert_eq!(
            relative_display(Path::new("/other/path/file.rs"), cwd),
            "/other/path/file.rs"
        );
        assert_eq!(relative_display(Path::new("/home/user/project"), cwd), "");
    }
}
