use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::cache::{ActionKey, ActionKeyInput, BuildCache, PublishOutcome, RECEIPT_VERSION, Receipt};
use crate::dag::{self, Dag, DagError, DagNode};
use crate::expr::{self, EvalError, Scope};
use crate::file_tracker::FileTracker;
use crate::loader::BaseScope;
use crate::output::{BlockWriter, Event, Output};
use crate::provider::{
    ApplyResult, BoxError, CachePolicy, OutputFile, PlanAction, PlanResult, ReceiptCheck, ResourceKind,
};
use crate::sha256::{Hasher, SHA256};
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
    #[error("{pos}: block '{block}' concurrency must be a positive integer, got {value}")]
    InvalidConcurrency {
        pos: crate::ast::Pos,
        block: String,
        value: String,
    },
}

#[derive(Clone)]
struct ConcurrencyLimit {
    group: String,
    limit: usize,
}

/// Result of planning a single block.
pub struct BlockPlan {
    pub name: String,
    pub plan: PlanResult,
}

/// Truncate a SHA256 for compact debug output.
fn short_hash(h: &SHA256) -> String {
    if h.is_zero() {
        return "<none>".to_owned();
    }
    let s = h.to_string();
    s[..8].to_owned()
}

/// Wrapped state persisted by the engine.
#[derive(serde::Serialize, serde::Deserialize)]
struct WrappedState {
    state: serde_json::Value,
    outputs: Map,
    content_hash: SHA256,
    #[serde(default)]
    input_hash: SHA256,
    #[serde(default)]
    resolve_map: BTreeMap<String, SHA256>,
    #[serde(default)]
    dep_hashes: BTreeMap<String, SHA256>,
}

/// Extracted prior state fields.
struct PriorState {
    provider_state: Option<serde_json::Value>,
    outputs: Map,
    content_hash: SHA256,
    input_hash: SHA256,
    resolve_map: BTreeMap<String, SHA256>,
    dep_hashes: BTreeMap<String, SHA256>,
}

fn unwrap_state(stored: &serde_json::Value) -> PriorState {
    let wrapped: WrappedState =
        serde_json::from_value(stored.clone()).expect("corrupted state: not a valid WrappedState");
    PriorState {
        provider_state: Some(wrapped.state),
        outputs: wrapped.outputs,
        content_hash: wrapped.content_hash,
        input_hash: wrapped.input_hash,
        resolve_map: wrapped.resolve_map,
        dep_hashes: wrapped.dep_hashes,
    }
}

fn restore_prior(writer: &BlockWriter, prior_state: Option<&serde_json::Value>) -> PriorState {
    match prior_state {
        Some(s) => {
            let prior = unwrap_state(s);
            crate::debug!(writer, "restored state (hash={})", short_hash(&prior.content_hash));
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
        content_hash: SHA256::ZERO,
        input_hash: SHA256::ZERO,
        resolve_map: BTreeMap::new(),
        dep_hashes: BTreeMap::new(),
    }
}

/// Hash the block's evaluated input fields in canonical (key-sorted) JSON
/// form, with project-internal paths made project-relative so the hash
/// agrees across worktrees.
fn hash_inputs(inputs: &Map, cache: &BuildCache) -> SHA256 {
    let canonical = serde_json::to_value(inputs)
        .map(|v| cache.normalize_json(v))
        .and_then(|v| serde_json::to_string(&v))
        .unwrap_or_default();
    SHA256::digest(canonical.as_bytes())
}

/// Compute a content hash from input hash, resolve map, and dependency hashes.
fn compute_content_hash(
    input_hash: &SHA256,
    resolve_map: &BTreeMap<String, SHA256>,
    dep_hashes: &BTreeMap<String, SHA256>,
) -> SHA256 {
    let mut hasher = Hasher::new();
    hasher.update(input_hash.to_string().as_bytes());
    for (key, hash) in resolve_map {
        hasher.update(key.as_bytes());
        hasher.update(hash.to_string().as_bytes());
    }
    for (dep, hash) in dep_hashes {
        hasher.update(dep.as_bytes());
        hasher.update(hash.to_string().as_bytes());
    }
    hasher.finalize()
}

/// Result hash of every block completed so far in the current run: the exact
/// result that was selected, restored, or produced. Dependents key on these
/// rather than on persistent state so parallel execution is deterministic
/// and a block never observes a stale hash from a previous run.
type RunResults = HashMap<String, SHA256>;

/// Collect dependency hashes from the current run. A dependency outside the
/// run's order falls back to the state loaded when the DAG was built.
fn collect_dep_hashes(dag: &Dag, block_name: &str, results: &RunResults) -> BTreeMap<String, SHA256> {
    let mut dep_hashes = BTreeMap::new();
    let mut deps = dag.content_deps(block_name);
    deps.sort();
    for dep in deps {
        let hash = results.get(&dep).copied().or_else(|| {
            dag.get_node(&dep)?
                .prior_state
                .as_ref()
                .map(|s| unwrap_state(s).content_hash)
        });
        if let Some(hash) = hash {
            dep_hashes.insert(dep, hash);
        }
    }
    dep_hashes
}

/// Describe why a block's content hash changed compared to its prior state.
fn change_reason(
    prior: &PriorState,
    new_resolve_map: &BTreeMap<String, SHA256>,
    new_input_hash: &SHA256,
    new_dep_hashes: &BTreeMap<String, SHA256>,
    dag: &Dag,
    block_name: &str,
    dirty_deps: &HashSet<String>,
) -> Option<String> {
    // Check for dirty dependency blocks.
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

    // Diff resolve maps.
    let mut changed = Vec::new();
    for (key, hash) in new_resolve_map {
        match prior.resolve_map.get(key) {
            Some(prior_hash) if prior_hash != hash => changed.push(format!("'{key}'")),
            None => changed.push(format!("'{key}' (new)")),
            _ => {}
        }
    }
    for key in prior.resolve_map.keys() {
        if !new_resolve_map.contains_key(key) {
            changed.push(format!("'{key}' (removed)"));
        }
    }
    if !changed.is_empty() {
        let count = changed.len();
        if count <= 3 {
            return Some(changed.join(", ") + " changed");
        }
        return Some(format!("{count} inputs changed"));
    }

    // Diff dependency result hashes (only meaningful when the prior state
    // recorded them).
    if !prior.dep_hashes.is_empty() {
        let changed_deps: Vec<_> = new_dep_hashes
            .iter()
            .filter(|(dep, hash)| prior.dep_hashes.get(*dep) != Some(*hash))
            .map(|(dep, _)| format!("'{dep}'"))
            .collect();
        if !changed_deps.is_empty() {
            return Some(changed_deps.join(", ") + " changed");
        }
    }

    // Fall back to input field changes.
    if !prior.input_hash.is_zero() && prior.input_hash != *new_input_hash {
        return Some("inputs changed".into());
    }

    None
}

pub fn plan_action_to_event(action: &PlanAction) -> Event {
    match action {
        PlanAction::Create => Event::Create,
        PlanAction::Update => Event::Update,
        PlanAction::Destroy => Event::Destroy,
        PlanAction::Restore => Event::Restore,
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
/// - empty → `default` target if defined, else every non-`explicit` block
/// - `...` (alone or alongside other targets) → every non-`explicit` block,
///   unioned with the orders of any other named targets/blocks (an `explicit`
///   block is included when named directly or pulled in as a dependency of
///   a selected target/block)
pub fn resolve_order(dag: &Dag, targets: &[String]) -> Result<Vec<String>, EngineError> {
    if targets.is_empty() {
        if dag.targets().contains_key("default") {
            return Ok(dag.target_order("default")?);
        }
        return Ok(dag.select_all()?);
    }
    let mut needed = HashSet::new();
    let mut wildcard = false;
    for t in targets {
        if t == "..." {
            wildcard = true;
            continue;
        }
        for name in dag.target_order(t)? {
            needed.insert(name);
        }
    }
    if wildcard {
        for name in dag.select_all()? {
            needed.insert(name);
        }
    }
    let all = dag.topo_order()?;
    Ok(all.into_iter().filter(|n| needed.contains(n)).collect())
}

/// Narrow an already-resolved execution order to blocks affected by a set of
/// changed paths. Directly affected blocks pull in their content dependents
/// and every prerequisite required to execute the result.
pub fn affected_order(
    dag: &Dag,
    base: &BaseScope,
    cache: &BuildCache,
    order: &[String],
    changes: &crate::git::Changes,
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<String>, EngineError> {
    if order.is_empty() {
        return Ok(Vec::new());
    }

    let normalize = |path: &std::path::Path| cache.normalize_str(&path.to_string_lossy()).into_owned();
    let changed: HashSet<String> = changes.paths.iter().map(|path| normalize(path)).collect();
    let mut unmatched_removed: HashSet<String> = changes.removed.iter().map(|path| normalize(path)).collect();

    // Build definitions can change ownership, dependencies, or evaluated
    // inputs in ways that cannot be attributed from the new graph alone.
    if changes.paths.iter().any(|path| {
        path.file_name().is_some_and(|name| name == "BUILD.bit.lock")
            || path.extension().is_some_and(|extension| extension == "bit")
    }) {
        return Ok(order.to_vec());
    }

    tracker.lock().expect("tracker lock").reset();
    let mut scope = base.scope.clone();
    let mut affected = HashSet::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let prior = node
            .prior_state
            .as_ref()
            .map(unwrap_state)
            .unwrap_or_else(default_prior);
        let mut direct = false;
        let mut current = BTreeMap::new();
        let mut outputs = HashSet::new();

        match eval_fields_lenient(&node.fields, &scope) {
            Ok(inputs) => {
                if prior.provider_state.is_some() && hash_inputs(&inputs, cache) != prior.input_hash {
                    direct = true;
                }
                match node.resource.resolve(&inputs) {
                    Ok(resolved) => current = cache.normalize_keys(&resolved),
                    Err(_) => direct = true,
                }
                match node.resource.output_keys(&inputs) {
                    Ok(keys) => {
                        outputs.extend(keys.into_iter().map(|key| cache.normalize_str(&key).into_owned()));
                    }
                    Err(_) => direct = true,
                }
            }
            Err(_) => direct = true,
        }

        let tracked: HashSet<&str> = current
            .keys()
            .chain(prior.resolve_map.keys())
            .map(String::as_str)
            .collect();
        for path in &tracked {
            if changed.contains(*path) {
                direct = true;
            }
            unmatched_removed.remove(*path);
        }

        // A block with no source paths cannot be proven unaffected by Git.
        if tracked.iter().all(|path| outputs.contains(*path)) {
            direct = true;
        }
        if dag
            .content_deps(name)
            .iter()
            .any(|dependency| affected.contains(dependency))
        {
            direct = true;
        }
        if direct {
            affected.insert(name.clone());
        }

        scope.set(name, Value::strct(prior.outputs));
    }

    // A removed path absent from both current and prior input maps may have
    // been an input in a clean checkout. Running the candidate order is the
    // only sound fallback.
    if !unmatched_removed.is_empty() {
        return Ok(order.to_vec());
    }

    let mut needed = affected.clone();
    for name in affected {
        for dependency in dag.target_order(&name)? {
            needed.insert(dependency);
        }
    }
    Ok(order.iter().filter(|name| needed.contains(*name)).cloned().collect())
}

/// A usable shared-cache receipt found for a block.
struct CacheHit {
    receipt: Receipt,
    check: ReceiptCheck,
}

/// Everything decided about a block before any side effect. Shared by plan
/// and apply so both report the same action.
struct Prepared {
    inputs: Map,
    prior: PriorState,
    /// Normalized resolve map (project-internal keys made relative).
    resolve_map: BTreeMap<String, SHA256>,
    input_hash: SHA256,
    dep_hashes: BTreeMap<String, SHA256>,
    content_hash: SHA256,
    plan: PlanResult,
    /// Declared outputs paired with their project-relative roles. Empty for
    /// resources the shared cache never consults.
    outputs: Vec<OutputFile>,
    /// The subset of `outputs` the shared cache stores and restores. Both an
    /// excluded output and a cached one are kept out of the action key, since
    /// neither is a source; only this subset reaches the CAS.
    cached_outputs: Vec<OutputFile>,
    /// Toolchain fingerprint; `Some` only for shared-cache resources whose
    /// fingerprint could be computed.
    toolchain: Option<BTreeMap<String, String>>,
    hit: Option<CacheHit>,
}

impl Prepared {
    /// Hash a dependent should use for this block if it is not going to run.
    fn settled_hash(&self) -> Option<SHA256> {
        match (&self.plan.action, &self.hit) {
            (PlanAction::None, Some(_)) => Some(self.content_hash),
            (PlanAction::None, None) => self.prior.provider_state.is_some().then_some(self.prior.content_hash),
            (PlanAction::Restore, Some(hit)) => Some(hit.receipt.content_hash),
            _ => None,
        }
    }
}

/// Block field listing outputs the shared cache must not store.
const UNCACHED_FIELD: &str = "uncached";

/// Pair each key a resource declares as an output with its project-relative
/// form. The relative form is what a receipt records, so that a receipt
/// written in one worktree is legible in every other; the key itself stays as
/// the destination, because that is the path this process can write.
fn output_files(node: &DagNode, inputs: &Map, cache: &BuildCache) -> Result<Vec<OutputFile>, BoxError> {
    Ok(node
        .resource
        .output_keys(inputs)?
        .into_iter()
        .map(|key| OutputFile {
            role: cache.normalize_str(&key).into_owned(),
            path: key,
        })
        .collect())
}

/// The entries of `uncached`. A trailing slash marks a directory in `output`,
/// so it is optional here too and matching ignores it.
fn uncached_outputs(inputs: &Map) -> HashSet<&str> {
    fn trim(s: &str) -> &str {
        s.trim_end_matches('/')
    }
    match inputs.get(UNCACHED_FIELD) {
        Some(Value::List(_, items)) => items.iter().filter_map(|v| v.as_str()).map(trim).collect(),
        Some(Value::Str(one)) => HashSet::from([trim(one)]),
        _ => HashSet::new(),
    }
}

/// Whether `uncached` names this output, under either of its spellings.
fn is_uncached(uncached: &HashSet<&str>, output: &OutputFile) -> bool {
    uncached.contains(output.path.trim_end_matches('/')) || uncached.contains(output.role.trim_end_matches('/'))
}

/// Compute the shared action key for a block from its normalized sources.
/// `resolve_map` is the (normalized) resolve map at the moment the key is
/// needed: before apply for lookup, after apply for publication.
fn action_key(
    name: &str,
    node: &DagNode,
    prepared: &Prepared,
    resolve_map: &BTreeMap<String, SHA256>,
    toolchain: &BTreeMap<String, String>,
    cache: &BuildCache,
) -> Result<ActionKey, BoxError> {
    let CachePolicy::Shared { version } = node.resource.cache_policy(&prepared.inputs) else {
        return Err("resource is not shared".into());
    };
    let produced: HashSet<&str> = prepared.outputs.iter().map(|o| o.role.as_str()).collect();
    let sources: BTreeMap<String, SHA256> = resolve_map
        .iter()
        .filter(|(k, _)| !produced.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    Ok(ActionKeyInput {
        receipt_version: RECEIPT_VERSION,
        provider: &node.provider,
        resource: &node.resource_name,
        cache_version: version,
        block: name,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        inputs: hash_inputs(&prepared.inputs, cache),
        sources: &sources,
        deps: &prepared.dep_hashes,
        toolchain,
    }
    .key())
}

fn provider_error(node: &DagNode, name: &str, phase: &'static str) -> impl FnOnce(BoxError) -> EngineError {
    let pos = node.pos.clone();
    let block = name.to_owned();
    move |source| EngineError::Provider {
        pos,
        block,
        phase,
        source,
    }
}

/// Resolve inputs, compute hashes, plan, and consult the shared cache.
#[allow(clippy::too_many_arguments)]
fn prepare_block(
    name: &str,
    node: &DagNode,
    dag: &Dag,
    inputs: Map,
    dep_hashes: BTreeMap<String, SHA256>,
    dirty: &HashSet<String>,
    cache: &BuildCache,
    writer: &BlockWriter,
) -> Result<Prepared, EngineError> {
    let prior = restore_prior(writer, node.prior_state.as_ref());

    let resolve_map = node
        .resource
        .resolve(&inputs)
        .map_err(provider_error(node, name, "resolve"))?;
    let resolve_map = cache.normalize_keys(&resolve_map);
    let input_hash = hash_inputs(&inputs, cache);
    let content_hash = compute_content_hash(&input_hash, &resolve_map, &dep_hashes);

    let has_dirty_dep = dag.content_deps(name).iter().any(|d| dirty.contains(d));
    let inputs_changed = has_dirty_dep || content_hash != prior.content_hash;
    crate::debug!(
        writer,
        "resolved {} key(s), hash={} prior_hash={} dirty_dep={} inputs_changed={}",
        resolve_map.len(),
        short_hash(&content_hash),
        short_hash(&prior.content_hash),
        has_dirty_dep,
        inputs_changed,
    );

    let previously_failed = node.resource.kind() == ResourceKind::Test
        && prior.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

    let mut plan = node
        .resource
        .plan(&inputs, prior.provider_state.as_ref())
        .map_err(provider_error(node, name, "plan"))?;

    if plan.action == PlanAction::None && (inputs_changed || previously_failed) && prior.provider_state.is_some() {
        plan.action = PlanAction::Update;
        if plan.reason.is_none() {
            plan.reason = if previously_failed {
                Some("previously failed".into())
            } else {
                change_reason(&prior, &resolve_map, &input_hash, &dep_hashes, dag, name, dirty)
            };
        }
    }

    if node.protected && plan.action == PlanAction::Destroy {
        return Err(EngineError::Protected {
            pos: node.pos.clone(),
            block: name.to_owned(),
            action: "destroyed",
        });
    }

    let mut prepared = Prepared {
        inputs,
        prior,
        resolve_map,
        input_hash,
        dep_hashes,
        content_hash,
        plan,
        outputs: Vec::new(),
        cached_outputs: Vec::new(),
        toolchain: None,
        hit: None,
    };

    if node.resource.cache_policy(&prepared.inputs) == CachePolicy::Local
        || !cache.is_shared()
        || prepared.plan.action == PlanAction::None
    {
        return Ok(prepared);
    }
    prepared.outputs = output_files(node, &prepared.inputs, cache).map_err(provider_error(node, name, "resolve"))?;
    let uncached = uncached_outputs(&prepared.inputs);
    prepared.cached_outputs = prepared
        .outputs
        .iter()
        .filter(|o| !is_uncached(&uncached, o))
        .cloned()
        .collect();
    let toolchain = match node.resource.toolchain(&prepared.inputs) {
        Ok(t) => t,
        Err(e) => {
            crate::debug!(writer, "toolchain fingerprint unavailable, cache disabled: {e}");
            return Ok(prepared);
        }
    };
    let key = action_key(name, node, &prepared, &prepared.resolve_map, &toolchain, cache)
        .map_err(provider_error(node, name, "resolve"))?;
    prepared.toolchain = Some(toolchain);

    // Look up a receipt only when every dependency hash is settled. A
    // previously failed test always reruns locally so the failure is
    // observed here rather than masked by another worktree's success.
    if previously_failed || has_dirty_dep {
        return Ok(prepared);
    }
    let Some(receipt) = cache.lookup(&key) else {
        crate::debug!(writer, "cache miss (key={})", short_hash(&key.digest()));
        return Ok(prepared);
    };
    let Some(cas) = cache.cas() else {
        return Ok(prepared);
    };
    let check = match node.resource.check_receipt(
        &prepared.inputs,
        &receipt.state,
        &prepared.cached_outputs,
        &receipt.artifacts,
        cas,
    ) {
        Ok(check) => check,
        Err(e) => {
            crate::debug!(writer, "receipt check failed: {e}");
            ReceiptCheck::Unusable
        }
    };
    crate::debug!(writer, "cache hit (key={}): {check:?}", short_hash(&key.digest()));
    match check {
        ReceiptCheck::Valid => prepared.plan.action = PlanAction::None,
        ReceiptCheck::Restore => prepared.plan.action = PlanAction::Restore,
        ReceiptCheck::Unusable => return Ok(prepared),
    }
    prepared.plan.reason = Some("cached".into());
    prepared.hit = Some(CacheHit { receipt, check });
    Ok(prepared)
}

/// Plan all blocks in the DAG (or a target subset), returning what would
/// change. Consults the shared cache read-only: a restorable receipt is
/// reported as [`PlanAction::Restore`] but nothing is materialized.
pub fn plan(
    dag: &mut Dag,
    base: &BaseScope,
    cache: &BuildCache,
    output: &Output,
    targets: &[String],
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    plan_selected(dag, base, cache, output, &order, tracker)
}

/// Plan an exact, already-resolved block order.
pub fn plan_selected(
    dag: &mut Dag,
    base: &BaseScope,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    tracker.lock().expect("tracker lock").reset();
    validate_active_params(dag, order, base)?;
    crate::debug!(output, "plan: {} block(s) in order: {}", order.len(), order.join(", "));

    let mut scope = base.scope.clone();
    let mut plans = Vec::new();
    let mut dirty: HashSet<String> = HashSet::new();
    let mut results = RunResults::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer_indented(name, 0);
        evaluate_concurrency(node, &scope)?;

        let inputs = eval_fields_lenient(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            block: name.clone(),
            source: e,
        })?;
        let dep_hashes = collect_dep_hashes(dag, name, &results);
        let prepared = prepare_block(name, node, dag, inputs, dep_hashes, &dirty, cache, &writer)?;

        match prepared.settled_hash() {
            Some(hash) => {
                results.insert(name.clone(), hash);
            }
            None => {
                dirty.insert(name.clone());
            }
        }

        crate::debug!(writer, "planned action: {:?}", prepared.plan.action);
        let event = plan_action_to_event(&prepared.plan.action);
        emit_event(
            &writer,
            event,
            &prepared.plan.description,
            prepared.plan.reason.as_deref(),
        );

        let outputs = match prepared.hit {
            Some(hit) => hit.receipt.outputs,
            None => prepared.prior.outputs,
        };
        scope.set(name, Value::strct(outputs));

        plans.push(BlockPlan {
            name: name.clone(),
            plan: prepared.plan,
        });
    }

    Ok(plans)
}

/// Apply all blocks in the DAG (or a target subset).
/// With no targets: runs the `default` target if defined, else all blocks.
/// With `["..."]`: runs all blocks.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    targets: &[String],
    jobs: usize,
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    apply_selected(dag, base, store, cache, output, &order, jobs, tracker)
}

/// Apply an exact, already-resolved block order.
#[allow(clippy::too_many_arguments)]
pub fn apply_selected(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    tracker.lock().expect("tracker lock").reset();
    validate_active_params(dag, order, base)?;
    crate::debug!(
        output,
        "apply: {} block(s), jobs={}, order: {}",
        order.len(),
        jobs,
        order.join(", ")
    );
    let concurrency = concurrency_limits(dag, &base.scope, order)?;
    if jobs <= 1 {
        apply_order(dag, base, store, cache, output, order, tracker)
    } else {
        apply_order_parallel(dag, base, store, cache, output, order, jobs, tracker, &concurrency)
    }
}

/// Apply only test blocks and their transitive dependencies.
#[allow(clippy::too_many_arguments)]
pub fn test(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    jobs: usize,
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let order = dag.test_order()?;
    crate::debug!(
        output,
        "test: {} block(s), jobs={}, order: {}",
        order.len(),
        jobs,
        order.join(", ")
    );
    apply_selected(dag, base, store, cache, output, &order, jobs, tracker)
}

/// Apply blocks sequentially in the given order.
fn apply_order(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    tracker: &Arc<Mutex<FileTracker>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    let mut scope = base.scope.clone();
    let mut results = RunResults::new();
    let mut plans = Vec::new();

    for name in order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);
        let dep_hashes = collect_dep_hashes(dag, name, &results);
        let result = execute_block(name, node, dag, &scope, dep_hashes, cache, &writer, tracker)?;
        let test_failed = result.test_failed;
        let pos = result.pos.clone();
        complete_block(result, store, output, &mut scope, &mut results, &mut plans)?;
        if test_failed {
            return Err(EngineError::TestFailed {
                pos,
                block: name.clone(),
            });
        }
    }

    Ok(plans)
}

/// Result sent back from a worker thread after executing a block.
struct BlockResult {
    pos: crate::ast::Pos,
    name: String,
    plan: PlanResult,
    outputs: Map,
    /// The wrapped state to persist, if the block was applied or restored.
    wrapped_state: Option<serde_json::Value>,
    /// Hash dependents should use for this block, if it has a settled result.
    result_hash: Option<SHA256>,
    /// Whether this was a failed test (passed == false).
    test_failed: bool,
}

/// Persist a completed block's state and make its result visible to
/// dependents.
fn complete_block(
    result: BlockResult,
    store: &dyn StateStore,
    output: &Output,
    scope: &mut Scope,
    results: &mut RunResults,
    plans: &mut Vec<BlockPlan>,
) -> Result<(), EngineError> {
    if let Some(state) = &result.wrapped_state {
        store.save(&result.name, state)?;
        crate::debug!(output.writer(&result.name), "saved state");
    }
    if let Some(hash) = result.result_hash {
        results.insert(result.name.clone(), hash);
    }
    scope.set(&result.name, Value::strct(result.outputs));
    plans.push(BlockPlan {
        name: result.name,
        plan: result.plan,
    });
    Ok(())
}

/// Apply blocks in parallel using a ready-queue scheduler.
#[allow(clippy::too_many_arguments)]
fn apply_order_parallel(
    dag: &Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<Mutex<FileTracker>>,
    concurrency: &HashMap<String, Option<ConcurrencyLimit>>,
) -> Result<Vec<BlockPlan>, EngineError> {
    use std::collections::VecDeque;
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
    let mut results = RunResults::new();
    let mut plans: Vec<BlockPlan> = Vec::new();
    let mut completed = 0;
    let total = order.len();

    std::thread::scope(|s| {
        let (result_tx, result_rx) = mpsc::channel::<(String, Result<BlockResult, EngineError>)>();
        let mut in_flight = 0;
        let mut running_by_group: HashMap<String, usize> = HashMap::new();
        let mut failed: Option<EngineError> = None;

        loop {
            // Dispatch ready blocks up to the job limit. Dependency hashes are
            // snapshotted here, on the scheduler thread, after every
            // dependency has completed.
            while in_flight < jobs && !ready.is_empty() && failed.is_none() {
                let Some(index) = ready.iter().position(|name| {
                    concurrency[name]
                        .as_ref()
                        .is_none_or(|config| running_by_group.get(&config.group).copied().unwrap_or(0) < config.limit)
                }) else {
                    break;
                };
                let name = ready.remove(index).expect("ready index is valid");
                let node = dag.get_node(&name).expect("block in order");
                let writer = output.writer(&name);
                let scope_snapshot = scope.clone();
                let dep_hashes = collect_dep_hashes(dag, &name, &results);
                let tx = result_tx.clone();
                let completed_name = name.clone();

                if let Some(config) = &concurrency[&name] {
                    *running_by_group.entry(config.group.clone()).or_default() += 1;
                }

                s.spawn(move || {
                    let result = execute_block(&name, node, dag, &scope_snapshot, dep_hashes, cache, &writer, tracker);
                    let _ = tx.send((completed_name, result));
                });
                in_flight += 1;
            }

            if in_flight == 0 {
                break;
            }

            // Wait for a result
            let (completed_name, result) = result_rx.recv().expect("channel open");
            in_flight -= 1;
            if let Some(config) = &concurrency[&completed_name]
                && let Some(running) = running_by_group.get_mut(&config.group)
            {
                *running -= 1;
            }

            match result {
                Ok(block_result) => {
                    let name = block_result.name.clone();
                    let test_failed = block_result.test_failed;
                    let pos = block_result.pos.clone();

                    if let Err(e) = complete_block(block_result, store, output, &mut scope, &mut results, &mut plans) {
                        failed = Some(e);
                        continue;
                    }

                    if test_failed {
                        failed = Some(EngineError::TestFailed {
                            pos,
                            block: name.clone(),
                        });
                    }
                    completed += 1;

                    // Unblock dependents
                    if failed.is_none() {
                        for dep in dag.dependents(&name) {
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
            None => Ok(plans),
        }
    })
}

/// Bind a receipt into the current context: recreate any missing artifacts
/// and obtain state and outputs valid for this worktree.
fn bind_receipt(
    node: &DagNode,
    inputs: &Map,
    outputs: &[OutputFile],
    hit: &CacheHit,
    cache: &BuildCache,
    writer: &BlockWriter,
) -> Result<ApplyResult<serde_json::Value, Map>, BoxError> {
    let cas = cache.cas().ok_or("shared cache unavailable")?;
    match node
        .resource
        .materialize(inputs, &hit.receipt.state, outputs, &hit.receipt.artifacts, cas, writer)?
    {
        Some(result) => Ok(result),
        // The default `materialize` recreates the declared outputs and leaves
        // the receipt's own state and outputs to be bound as they are. A
        // resource that asks for a restore while declaring nothing to restore
        // has not said how to perform one.
        None if hit.check == ReceiptCheck::Restore && outputs.is_empty() => {
            Err("resource requested restore but did not materialize".into())
        }
        None => Ok(ApplyResult {
            outputs: hit.receipt.outputs.clone(),
            state: Some(hit.receipt.state.clone()),
        }),
    }
}

/// Store a successful action's artifacts in the CAS and publish its receipt
/// under the post-apply key. Best-effort: failures warn but never fail the
/// build.
#[allow(clippy::too_many_arguments)]
fn publish_receipt(
    name: &str,
    node: &DagNode,
    prepared: &Prepared,
    post_resolve: &BTreeMap<String, SHA256>,
    post_hash: SHA256,
    state: &serde_json::Value,
    outputs: &Map,
    toolchain: &BTreeMap<String, String>,
    cache: &BuildCache,
    writer: &BlockWriter,
) {
    let CachePolicy::Shared { version } = node.resource.cache_policy(&prepared.inputs) else {
        return;
    };
    let Some(cas) = cache.cas() else {
        return;
    };
    let attempt = || -> Result<(ActionKey, PublishOutcome), BoxError> {
        let artifacts = node
            .resource
            .capture_artifacts(&prepared.inputs, state, &prepared.cached_outputs, cas)?;
        let key = action_key(name, node, prepared, post_resolve, toolchain, cache)?;
        let receipt = Receipt {
            version: RECEIPT_VERSION,
            provider: node.provider.clone(),
            resource: node.resource_name.clone(),
            cache_version: version,
            state: state.clone(),
            outputs: outputs.clone(),
            artifacts,
            content_hash: post_hash,
        };
        Ok((key, cache.publish(&key, &receipt)?))
    };
    match attempt() {
        Ok((key, PublishOutcome::Published)) => {
            crate::debug!(writer, "published receipt (key={})", short_hash(&key.digest()));
        }
        Ok((_, PublishOutcome::AlreadyPresent)) => {}
        Ok((key, PublishOutcome::Conflict)) => writer.stderr_line(&format!(
            "warning: a different receipt already exists for this action (key={}); keeping it. \
             The action key may be missing an input the result depends on.",
            short_hash(&key.digest())
        )),
        Err(e) => writer.stderr_line(&format!("warning: could not publish to the shared cache: {e}")),
    }
}

/// Execute a single block: evaluate fields, plan, restore or apply if
/// needed, compute the post-apply hash, and publish a receipt.
#[allow(clippy::too_many_arguments)]
fn execute_block(
    name: &str,
    node: &DagNode,
    dag: &Dag,
    scope: &Scope,
    dep_hashes: BTreeMap<String, SHA256>,
    cache: &BuildCache,
    writer: &BlockWriter,
    tracker: &Mutex<FileTracker>,
) -> Result<BlockResult, EngineError> {
    let inputs = eval_fields(&node.fields, scope).map_err(|e| EngineError::Eval {
        pos: node.pos.clone(),
        block: name.to_owned(),
        source: e,
    })?;

    let mut prepared = prepare_block(name, node, dag, inputs, dep_hashes, &HashSet::new(), cache, writer)?;
    crate::debug!(writer, "planned action: {:?}", prepared.plan.action);

    // Bind or restore from a shared receipt. Any failure here is a cache
    // miss: fall through to running the provider.
    let mut bound: Option<ApplyResult<serde_json::Value, Map>> = None;
    if let Some(hit) = &prepared.hit {
        if hit.check == ReceiptCheck::Restore {
            emit_event(
                writer,
                Event::Restore,
                &prepared.plan.description,
                prepared.plan.reason.as_deref(),
            );
        }
        match bind_receipt(node, &prepared.inputs, &prepared.cached_outputs, hit, cache, writer) {
            Ok(result) => bound = Some(result),
            Err(e) => {
                writer.stderr_line(&format!("warning: cannot use cached result, running instead: {e}"));
                prepared.hit = None;
                prepared.plan.action = if prepared.prior.provider_state.is_some() {
                    PlanAction::Update
                } else {
                    PlanAction::Create
                };
                prepared.plan.reason = None;
            }
        }
    }

    if bound.is_none() && prepared.plan.action == PlanAction::None {
        writer.event(Event::Skipped, "no changes");
        let result_hash = prepared.settled_hash();
        return Ok(BlockResult {
            pos: node.pos.clone(),
            name: name.to_owned(),
            plan: prepared.plan,
            outputs: prepared.prior.outputs,
            wrapped_state: None,
            result_hash,
            test_failed: false,
        });
    }

    let from_cache = bound.is_some();
    let apply_result = match bound {
        Some(result) => result,
        None => {
            emit_event(
                writer,
                Event::Starting,
                &prepared.plan.description,
                prepared.plan.reason.as_deref(),
            );
            node.resource
                .apply(&prepared.inputs, prepared.prior.provider_state.as_ref(), writer)
                .map_err(|e| {
                    writer.event(Event::Failed, &e.to_string());
                    EngineError::Provider {
                        pos: node.pos.clone(),
                        block: name.to_owned(),
                        phase: "apply",
                        source: e,
                    }
                })?
        }
    };

    let test_failed = node.resource.kind() == ResourceKind::Test
        && apply_result.outputs.get("passed").and_then(|v| v.as_bool()) == Some(false);

    let mut result_hash = None;
    let wrapped_state = if let Some(new_state) = &apply_result.state {
        // Re-resolve after apply: outputs now exist and mutating actions
        // such as formatters change their own inputs.
        tracker.lock().expect("tracker lock").clear_hash_cache();
        let post_resolve = cache.normalize_keys(&node.resource.resolve(&prepared.inputs).unwrap_or_default());
        let post_hash = compute_content_hash(&prepared.input_hash, &post_resolve, &prepared.dep_hashes);
        crate::debug!(writer, "post-apply hash={}", short_hash(&post_hash));
        result_hash = Some(post_hash);

        // Failed tests stay in local state so they rerun, but never enter
        // the shared cache.
        if !from_cache
            && !test_failed
            && let Some(toolchain) = &prepared.toolchain
        {
            publish_receipt(
                name,
                node,
                &prepared,
                &post_resolve,
                post_hash,
                new_state,
                &apply_result.outputs,
                toolchain,
                cache,
                writer,
            );
        }

        let wrapped = WrappedState {
            state: new_state.clone(),
            outputs: apply_result.outputs.clone(),
            content_hash: post_hash,
            input_hash: prepared.input_hash,
            resolve_map: post_resolve,
            dep_hashes: prepared.dep_hashes,
        };
        Some(serde_json::to_value(&wrapped).expect("serialize wrapped state"))
    } else {
        crate::debug!(writer, "apply returned no state (stateless block)");
        None
    };

    if test_failed {
        writer.event(Event::Failed, "tests failed");
    } else if from_cache && prepared.plan.action == PlanAction::None {
        writer.event(Event::Skipped, "cached");
    } else {
        writer.event(Event::Ok, "");
    }

    Ok(BlockResult {
        pos: node.pos.clone(),
        name: name.to_owned(),
        plan: prepared.plan,
        outputs: apply_result.outputs,
        wrapped_state,
        result_hash,
        test_failed,
    })
}

/// Destroy each target block and all of its transitive dependents, in reverse
/// topological order (sinks first, so each block is torn down before the
/// blocks it depends on).
///
/// # Arguments
///
/// * `targets` - Block or target names to destroy. An empty slice, or any
///   target list containing the literal `"..."`, destroys every non-`explicit`
///   block in the DAG (unioned with the transitive dependents of the other
///   named targets); `explicit` blocks must be named to be destroyed.
/// * `force` - When true, destroy protected blocks and continue past provider
///   errors (the failing block still shows an error, but remaining blocks
///   proceed).
///
/// # Errors
///
/// Returns [`EngineError`] if a target is unknown, a cycle exists, or a
/// provider fails during teardown (unless `force` is set, in which case
/// provider errors are collected and the first is returned after all blocks
/// have been processed).
pub fn destroy(
    dag: &mut Dag,
    store: &dyn StateStore,
    output: &Output,
    targets: &[String],
    force: bool,
) -> Result<(), EngineError> {
    let mut order: Vec<String> = if targets.is_empty() {
        dag.select_all()?
    } else {
        // Gather each target's transitive dependents (inclusive) and emit them
        // in a single topological order so tie-breaking stays deterministic.
        // `...` is composable: it contributes every non-`explicit` block,
        // unioned with the dependents of any other named targets.
        let mut needed = std::collections::HashSet::new();
        let mut wildcard = false;
        for t in targets {
            if t == "..." {
                wildcard = true;
                continue;
            }
            for name in dag.transitive_dependents(t)? {
                needed.insert(name);
            }
        }
        if wildcard {
            for name in dag.select_all()? {
                needed.insert(name);
            }
        }
        dag.topo_order()?.into_iter().filter(|n| needed.contains(n)).collect()
    };
    order.reverse();
    crate::debug!(
        output,
        "destroy: {} block(s) in reverse order: {}",
        order.len(),
        order.join(", ")
    );

    let mut first_error: Option<EngineError> = None;

    for name in &order {
        let node = dag.get_node(name).ok_or_else(|| DagError::UnknownBlock(name.clone()))?;
        let writer = output.writer(name);

        if node.protected && !force {
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

        if let Err(e) = node.resource.destroy(&provider_state, &writer) {
            let err = EngineError::Provider {
                pos: node.pos.clone(),
                block: name.clone(),
                phase: "destroy",
                source: e,
            };
            writer.event(Event::Failed, &format!("{err}"));
            if !force {
                return Err(err);
            }
            if first_error.is_none() {
                first_error = Some(err);
            }
        }

        store.remove(name)?;
        crate::debug!(writer, "removed state from store");
        writer.event(Event::Ok, "");
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Dump evaluated inputs and stored outputs for all blocks (or a target subset).
pub fn dump(dag: &mut Dag, base: &BaseScope, targets: &[String]) -> Result<(), EngineError> {
    let order = resolve_order(dag, targets)?;
    dump_selected(dag, base, &order)
}

/// Dump evaluated inputs and stored outputs for an exact block order.
pub fn dump_selected(dag: &mut Dag, base: &BaseScope, order: &[String]) -> Result<(), EngineError> {
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
        if field.name == "concurrency" {
            continue;
        }
        let value = expr::eval(&field.value, scope)?;
        inputs.insert(field.name.clone(), value);
    }
    Ok(inputs)
}

fn eval_fields_lenient(fields: &[crate::ast::Field], scope: &Scope) -> Result<Map, EvalError> {
    let mut inputs = Map::new();
    for field in fields {
        if field.name == "concurrency" {
            continue;
        }
        let value = expr::eval_lenient(&field.value, scope)?;
        inputs.insert(field.name.clone(), value);
    }
    Ok(inputs)
}

fn evaluate_concurrency(node: &DagNode, scope: &Scope) -> Result<Option<ConcurrencyLimit>, EngineError> {
    let Some(field) = node.fields.iter().find(|field| field.name == "concurrency") else {
        return Ok(None);
    };
    let value = expr::eval(&field.value, scope).map_err(|source| EngineError::Eval {
        pos: node.pos.clone(),
        block: node.name.clone(),
        source,
    })?;
    let limit = value
        .as_number()
        .and_then(|number| number.to_string().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .ok_or_else(|| EngineError::InvalidConcurrency {
            pos: node.pos.clone(),
            block: node.name.clone(),
            value: value.to_string(),
        })?;
    Ok(Some(ConcurrencyLimit {
        group: node.concurrency_group.clone(),
        limit,
    }))
}

fn concurrency_limits(
    dag: &Dag,
    scope: &Scope,
    order: &[String],
) -> Result<HashMap<String, Option<ConcurrencyLimit>>, EngineError> {
    order
        .iter()
        .map(|name| {
            let node = dag.get_node(name).expect("block in order");
            Ok((name.clone(), evaluate_concurrency(node, scope)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::file_tracker::FileTracker;
    use crate::loader;
    use crate::parser;
    use crate::provider::{DynResource, FuncSignature, Provider, ProviderRegistry, Resource};
    use crate::providers::exec::ExecProvider;
    use crate::state::StateStore;

    fn test_tracker() -> Arc<Mutex<FileTracker>> {
        Arc::new(Mutex::new(FileTracker::new()))
    }

    fn test_cache() -> BuildCache {
        BuildCache::local_only(std::path::Path::new("."))
    }

    fn test_registry(tracker: &Arc<Mutex<FileTracker>>) -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(ExecProvider::new(tracker.clone())));
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
        let tracker = test_tracker();
        let module = parser::parse(input, "<test>").expect("parse failed");
        let store = MemoryStore::new();
        let (mut dag, base) =
            loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).expect("load failed");
        let output = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker)
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
    fn matrix_concurrency_does_not_limit_unrelated_blocks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Counts {
            running: AtomicUsize,
            max_running: AtomicUsize,
            matrix_running: AtomicUsize,
            max_matrix_running: AtomicUsize,
        }

        #[derive(Debug, serde::Deserialize, bit_derive::Schema)]
        struct Inputs {
            label: String,
        }

        #[derive(Debug, serde::Serialize, bit_derive::Schema)]
        struct Outputs {}

        struct ProbeProvider {
            counts: Arc<Counts>,
        }

        impl Provider for ProbeProvider {
            fn name(&self) -> &str {
                "probe"
            }

            fn resources(&self) -> Vec<Box<dyn DynResource>> {
                vec![Box::new(ProbeResource {
                    counts: self.counts.clone(),
                })]
            }

            fn functions(&self) -> Vec<FuncSignature> {
                vec![]
            }

            fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
                Err(format!("unknown function: {name}").into())
            }
        }

        struct ProbeResource {
            counts: Arc<Counts>,
        }

        impl Resource for ProbeResource {
            type State = ();
            type Inputs = Inputs;
            type Outputs = Outputs;

            fn name(&self) -> &str {
                "run"
            }

            fn kind(&self) -> ResourceKind {
                ResourceKind::Build
            }

            fn resolve(&self, _inputs: &Inputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
                Ok(BTreeMap::new())
            }

            fn plan(&self, _inputs: &Inputs, _prior_state: Option<&()>) -> Result<PlanResult, BoxError> {
                Ok(PlanResult {
                    action: PlanAction::Create,
                    description: "probe concurrency".into(),
                    reason: None,
                })
            }

            fn apply(
                &self,
                inputs: &Inputs,
                _prior_state: Option<&()>,
                _writer: &BlockWriter,
            ) -> Result<ApplyResult<(), Outputs>, BoxError> {
                let running = self.counts.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.counts.max_running.fetch_max(running, Ordering::SeqCst);

                let is_matrix = inputs.label.starts_with("matrix-");
                if is_matrix {
                    let matrix_running = self.counts.matrix_running.fetch_add(1, Ordering::SeqCst) + 1;
                    self.counts
                        .max_matrix_running
                        .fetch_max(matrix_running, Ordering::SeqCst);
                }

                std::thread::sleep(std::time::Duration::from_millis(40));

                if is_matrix {
                    self.counts.matrix_running.fetch_sub(1, Ordering::SeqCst);
                }
                self.counts.running.fetch_sub(1, Ordering::SeqCst);
                Ok(ApplyResult {
                    outputs: Outputs {},
                    state: Some(()),
                })
            }

            fn destroy(&self, _prior_state: &(), _writer: &BlockWriter) -> Result<(), BoxError> {
                Ok(())
            }

            fn cache_policy(&self, _inputs: &Inputs) -> CachePolicy {
                CachePolicy::Local
            }
        }

        let input = r#"
let package = ["a", "b", "c"]

work[package] = probe.run {
  concurrency = 1
  label = "matrix-#{package}"
}

other-a = probe.run { label = "other-a" }
other-b = probe.run { label = "other-b" }
"#;
        let tracker = test_tracker();
        let counts = Arc::new(Counts::default());
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(ProbeProvider { counts: counts.clone() }));
        let module = parser::parse(input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &registry, &store, &[]).unwrap();

        let matrix_node = dag.get_node("work[a]").unwrap();
        assert_eq!(matrix_node.concurrency_group, "work");
        assert!(
            !eval_fields(&matrix_node.fields, &base.scope)
                .unwrap()
                .contains_key("concurrency")
        );

        let output = Output::new(&[]);
        let plans = apply(&mut dag, &base, &store, &test_cache(), &output, &[], 4, &tracker).unwrap();
        assert_eq!(plans.len(), 5);
        assert_eq!(counts.max_matrix_running.load(Ordering::SeqCst), 1);
        assert!(counts.max_running.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn concurrency_must_be_a_positive_integer() {
        let tracker = test_tracker();
        let module = parser::parse(r#"build = exec { concurrency = 0 command = "true" }"#, "<test>").unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let output = Output::new(&[]);
        let error = apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker)
            .err()
            .expect("invalid concurrency should fail");
        assert!(matches!(error, EngineError::InvalidConcurrency { .. }));
    }

    #[test]
    fn apply_chain_passes_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hi\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        let plans = plan(&mut dag, &base, &test_cache(), &out, &[], &tracker).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].plan.action, PlanAction::Create);
    }

    #[test]
    fn destroy_removes_state() {
        let tracker = test_tracker();
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
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();
        assert!(!store.list().unwrap().is_empty());

        // Reload with state, then destroy
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &[], false).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn protected_block_skips_destroy() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &[], false).unwrap();
        // State should still exist — destroy was skipped
        assert!(!store.list().unwrap().is_empty());
    }

    #[test]
    fn force_destroys_protected_block() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();
        assert!(!store.list().unwrap().is_empty());

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &[], true).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn force_continues_past_destroy_errors() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let out_a = dir.path().join("a.txt");
        let out_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n  clean = \"false\"\n}}\n",
                "b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            out_a.display(),
            out_a.display(),
            out_b.display(),
            out_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        // Without force, first error stops everything
        assert!(destroy(&mut dag, &store, &out, &[], false).is_err());

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        // With force, still returns error but processes all blocks
        let result = destroy(&mut dag, &store, &out, &[], true);
        assert!(result.is_err());
        // "b" should have been cleaned despite "a" failing first
        // (reverse order: b first, then a — a fails but b already succeeded)
        // Both states should be removed since we remove state even on error in force mode
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn explicit_block_excluded_from_wildcard_selection() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();

        // `bit` (no targets, no default) skips explicit blocks
        assert_eq!(resolve_order(&dag, &[]).unwrap(), vec!["a".to_owned()]);
        // `bit ...` also skips explicit blocks
        assert_eq!(resolve_order(&dag, &["...".into()]).unwrap(), vec!["a".to_owned()]);
        // Naming the block directly still runs it
        assert_eq!(resolve_order(&dag, &["b".into()]).unwrap(), vec!["b".to_owned()]);
    }

    #[test]
    fn affected_order_propagates_to_content_dependents_and_adds_prerequisites() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        std::fs::write(&c, "c").unwrap();
        let input = format!(
            concat!(
                "a = exec {{ command = \"true\" inputs = [\"{}\"] }}\n",
                "b = exec {{ command = \"true\" inputs = [\"{}\"] }}\n",
                "c = exec {{ command = \"true\" inputs = [\"{}\"] depends_on = [a] }}\n",
            ),
            a.display(),
            b.display(),
            c.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let order = resolve_order(&dag, &[]).unwrap();
        let cache = BuildCache::local_only(dir.path());

        let changed_a = crate::git::Changes {
            paths: HashSet::from([a.clone()]),
            removed: HashSet::new(),
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &changed_a, &tracker).unwrap(),
            vec!["a".to_owned(), "c".to_owned()]
        );

        let changed_c = crate::git::Changes {
            paths: HashSet::from([c]),
            removed: HashSet::new(),
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &changed_c, &tracker).unwrap(),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn affected_order_keeps_unmapped_blocks_and_falls_back_for_unknown_deletions() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        std::fs::write(&source, "source").unwrap();
        let input = format!(
            concat!(
                "mapped = exec {{ command = \"true\" inputs = [\"{}\"] }}\n",
                "unmapped = exec {{ command = \"true\" }}\n",
            ),
            source.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let order = resolve_order(&dag, &[]).unwrap();
        let cache = BuildCache::local_only(dir.path());

        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &crate::git::Changes::default(), &tracker,).unwrap(),
            vec!["unmapped".to_owned()]
        );

        let removed = dir.path().join("removed.txt");
        let deletion = crate::git::Changes {
            paths: HashSet::from([removed.clone()]),
            removed: HashSet::from([removed]),
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &deletion, &tracker).unwrap(),
            order
        );
    }

    #[test]
    fn explicit_block_pulled_in_as_dependency() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        // `b` depends on `a`; `a` is explicit. Selecting `b` must still pull in `a`.
        let input = format!(
            concat!(
                "explicit a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();

        assert_eq!(
            resolve_order(&dag, &["b".into()]).unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        // `...` still skips the explicit dep, leaving b without its dependency —
        // matching the contract that `...` is purely a "non-explicit blocks" filter.
        assert_eq!(resolve_order(&dag, &["...".into()]).unwrap(), vec!["b".to_owned()]);
        // `...` is composable: naming the explicit block alongside `...` unions
        // the wildcard expansion with the named block (and its deps).
        assert_eq!(
            resolve_order(&dag, &["...".into(), "a".into()]).unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn wildcard_composes_with_named_target() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        // `a` is a regular block selected by `...`; `b` is explicit and must
        // be named to participate.
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();

        // `bit ... b` should run every non-explicit block plus the named explicit one.
        let mut order = resolve_order(&dag, &["...".into(), "b".into()]).unwrap();
        order.sort();
        assert_eq!(order, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn explicit_block_skipped_by_destroy_wildcard() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        // Apply both directly so state is populated for both.
        apply(
            &mut dag,
            &base,
            &store,
            &test_cache(),
            &out,
            &["a".into(), "b".into()],
            1,
            &tracker,
        )
        .unwrap();
        let mut stored: Vec<String> = store.list().unwrap();
        stored.sort();
        assert_eq!(stored, vec!["a".to_owned(), "b".to_owned()]);

        // Destroying with `...` should leave the explicit block's state in place.
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["...".into()], false).unwrap();
        assert_eq!(store.list().unwrap(), vec!["b".to_owned()]);
    }

    #[test]
    fn destroy_wildcard_composes_with_named_block() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(
            &mut dag,
            &base,
            &store,
            &test_cache(),
            &out,
            &["a".into(), "b".into()],
            1,
            &tracker,
        )
        .unwrap();

        // `bit -c ... b` should destroy every non-explicit block AND the named explicit one.
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["...".into(), "b".into()], false).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn destroy_target_removes_only_target_and_dependents() {
        let tracker = test_tracker();
        // Build a chain a -> b -> c (c depends on b depends on a), apply all,
        // then destroy "b" and assert a survives while b and c are removed.
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let file_c = dir.path().join("c.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "c = exec {{\n  command = \"cp #{{b.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
            file_c.display(),
            file_c.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();
        let mut stored: Vec<String> = store.list().unwrap();
        stored.sort();
        assert_eq!(stored, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);

        // Cleaning "b" should remove b and its dependent c, but leave a alone.
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["b".into()], false).unwrap();
        assert_eq!(store.list().unwrap(), vec!["a".to_owned()]);
    }

    #[test]
    fn destroy_leaf_target_does_not_touch_dependencies() {
        let tracker = test_tracker();
        // Destroying the leaf c must leave both a and b intact — the old
        // behaviour walked dependencies and would have removed them too.
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let file_c = dir.path().join("c.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
                "c = exec {{\n  command = \"cp #{{b.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
            file_c.display(),
            file_c.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["c".into()], false).unwrap();
        let mut stored: Vec<String> = store.list().unwrap();
        stored.sort();
        assert_eq!(stored, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn target_filters_blocks() {
        let tracker = test_tracker();
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
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        let results = apply(
            &mut dag,
            &base,
            &store,
            &test_cache(),
            &out,
            &["just_a".into()],
            1,
            &tracker,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "a");
        assert!(out_a.exists());
        assert!(!out_b.exists());
    }

    #[test]
    fn timestamp_fast_path_persisted() {
        let tracker = test_tracker();
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
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        // Verify persisted state has timestamps
        let stored = store.load("build").unwrap().unwrap();
        let wrapped: WrappedState = serde_json::from_value(stored).unwrap();
        assert!(!wrapped.content_hash.is_zero());
        assert!(!wrapped.resolve_map.is_empty());

        // Second apply should be a no-op (timestamp fast path)
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let results = apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::None);
    }

    #[test]
    fn timestamp_detects_file_change() {
        let tracker = test_tracker();
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
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        // Modify input file (touch with new content to change both mtime and hash)
        std::fs::write(&input_file, "v2").unwrap();

        // Second apply should detect the change
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let results = apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::Update);
    }

    // -- Shared cache -----------------------------------------------------

    mod cached {
        //! Test provider whose `file` resource writes `<dir>/out.txt` from
        //! `<dir>/src.txt` (or a literal `content`) and shares the output
        //! through the CAS. `check` is the same resource as a test kind whose
        //! `passed` output is controlled by an input.
        use std::collections::BTreeMap;
        use std::path::{Path, PathBuf};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use serde::{Deserialize, Serialize};

        use crate::cache::{ArtifactRef, Cas};
        use crate::output::BlockWriter;
        use crate::provider::{
            ApplyResult, BoxError, CachePolicy, DynResource, FuncSignature, MaterializeResult, OutputFile, PlanAction,
            PlanResult, Provider, Resource, ResourceKind,
        };
        use crate::sha256::SHA256;
        use crate::value::Value;

        #[derive(Debug, Deserialize, bit_derive::Schema)]
        pub struct Inputs {
            pub dir: String,
            #[serde(default)]
            pub content: Option<String>,
            #[serde(default)]
            pub passed: Option<bool>,
        }

        #[derive(Debug, Serialize, bit_derive::Schema)]
        pub struct Outputs {
            pub path: String,
            pub passed: bool,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct State {
            pub path: String,
        }

        pub struct CachedProvider {
            pub applies: Arc<AtomicUsize>,
        }

        impl Provider for CachedProvider {
            fn name(&self) -> &str {
                "cached"
            }
            fn resources(&self) -> Vec<Box<dyn DynResource>> {
                vec![
                    Box::new(FileResource {
                        applies: self.applies.clone(),
                        kind: ResourceKind::Build,
                        name: "file",
                    }),
                    Box::new(FileResource {
                        applies: self.applies.clone(),
                        kind: ResourceKind::Test,
                        name: "check",
                    }),
                ]
            }
            fn functions(&self) -> Vec<FuncSignature> {
                vec![]
            }
            fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
                Err(format!("no function {name}").into())
            }
        }

        pub struct FileResource {
            applies: Arc<AtomicUsize>,
            kind: ResourceKind,
            name: &'static str,
        }

        fn out_path(inputs: &Inputs) -> PathBuf {
            Path::new(&inputs.dir).join("out.txt")
        }

        impl Resource for FileResource {
            type State = State;
            type Inputs = Inputs;
            type Outputs = Outputs;

            fn name(&self) -> &str {
                self.name
            }
            fn kind(&self) -> ResourceKind {
                self.kind.clone()
            }
            fn resolve(&self, inputs: &Inputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
                let mut map = BTreeMap::new();
                for name in ["src.txt", "out.txt"] {
                    let path = Path::new(&inputs.dir).join(name);
                    if path.is_file() {
                        map.insert(path.to_string_lossy().into_owned(), crate::providers::hash_file(&path)?);
                    }
                }
                Ok(map)
            }
            fn plan(&self, _inputs: &Inputs, prior: Option<&State>) -> Result<PlanResult, BoxError> {
                Ok(PlanResult {
                    action: if prior.is_some() {
                        PlanAction::None
                    } else {
                        PlanAction::Create
                    },
                    description: "write out.txt".into(),
                    reason: None,
                })
            }
            fn apply(
                &self,
                inputs: &Inputs,
                _prior: Option<&State>,
                _writer: &BlockWriter,
            ) -> Result<ApplyResult<State, Outputs>, BoxError> {
                self.applies.fetch_add(1, Ordering::SeqCst);
                let out = out_path(inputs);
                let content = match &inputs.content {
                    Some(c) => c.clone(),
                    None => std::fs::read_to_string(Path::new(&inputs.dir).join("src.txt"))?,
                };
                std::fs::write(&out, content)?;
                std::fs::set_permissions(&out, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
                Ok(ApplyResult {
                    outputs: Outputs {
                        path: out.to_string_lossy().into_owned(),
                        passed: inputs.passed.unwrap_or(true),
                    },
                    state: Some(State {
                        path: out.to_string_lossy().into_owned(),
                    }),
                })
            }
            fn destroy(&self, state: &State, _writer: &BlockWriter) -> Result<(), BoxError> {
                let _ = std::fs::remove_file(&state.path);
                Ok(())
            }
            fn cache_policy(&self, _inputs: &Inputs) -> CachePolicy {
                CachePolicy::Shared { version: 1 }
            }
            fn output_keys(&self, inputs: &Inputs) -> Vec<String> {
                vec![out_path(inputs).to_string_lossy().into_owned()]
            }
            fn toolchain(&self, _inputs: &Inputs) -> Result<BTreeMap<String, String>, BoxError> {
                Ok(BTreeMap::from([("tool".to_owned(), "1".to_owned())]))
            }
            /// Capture and check come from the default implementations. Only
            /// the state and outputs are rebuilt here, because they hold this
            /// worktree's absolute output path.
            fn materialize(
                &self,
                inputs: &Inputs,
                _state: &State,
                outputs: &[OutputFile],
                artifacts: &BTreeMap<String, ArtifactRef>,
                cas: &Cas,
                writer: &BlockWriter,
            ) -> MaterializeResult<State, Outputs> {
                crate::provider::restore_output_files(outputs, artifacts, cas, writer)?;
                let path = out_path(inputs).to_string_lossy().into_owned();
                Ok(Some(ApplyResult {
                    outputs: Outputs {
                        path: path.clone(),
                        passed: true,
                    },
                    state: Some(State { path }),
                }))
            }
        }
    }

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One linked git worktree: a project root with its own local state store.
    struct Worktree {
        root: PathBuf,
        store: MemoryStore,
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::git::command()
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git available");
        assert!(status.success(), "git {args:?} failed");
    }

    impl Worktree {
        fn new(repo: &Path, base: &Path, name: &str) -> Self {
            let root = base.join(name);
            git(repo, &["worktree", "add", "-q", "--detach", root.to_str().unwrap()]);
            Self {
                root: root.canonicalize().unwrap(),
                store: MemoryStore::new(),
            }
        }

        fn write_src(&self, content: &str) {
            std::fs::write(self.root.join("src.txt"), content).unwrap();
        }

        fn out(&self) -> PathBuf {
            self.root.join("out.txt")
        }

        fn block(&self, name: &str, resource: &str, extra: &str) -> String {
            format!(
                "{name} = cached.{resource} {{\n  dir = \"{}\"\n{extra}}}\n",
                self.root.display()
            )
        }

        fn delete(&self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        repo: PathBuf,
        cache_dir: PathBuf,
        applies: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let repo = base.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            git(
                &repo,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "init",
                ],
            );
            Self {
                cache_dir: base.join("cache"),
                base,
                repo,
                _tmp: tmp,
                applies: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn worktree(&self, name: &str) -> Worktree {
            Worktree::new(&self.repo, &self.base, name)
        }

        fn registry(&self) -> ProviderRegistry {
            let mut reg = ProviderRegistry::new();
            reg.register(Box::new(cached::CachedProvider {
                applies: self.applies.clone(),
            }));
            reg
        }

        fn cache(&self, wt: &Worktree) -> BuildCache {
            BuildCache::open_at(&wt.root, &self.cache_dir)
        }

        fn load(&self, wt: &Worktree, input: &str) -> (Dag, BaseScope, Arc<Mutex<FileTracker>>) {
            let tracker = test_tracker();
            let module = parser::parse(input, "<test>").expect("parse failed");
            let (dag, base) =
                loader::load(&module, &Map::new(), &self.registry(), &wt.store, &[]).expect("load failed");
            (dag, base, tracker)
        }

        fn apply(&self, wt: &Worktree, input: &str, jobs: usize) -> Result<Vec<BlockPlan>, EngineError> {
            let (mut dag, base, tracker) = self.load(wt, input);
            let output = Output::new(&[]);
            apply(
                &mut dag,
                &base,
                &wt.store,
                &self.cache(wt),
                &output,
                &[],
                jobs,
                &tracker,
            )
        }

        fn plan(&self, wt: &Worktree, input: &str) -> Vec<BlockPlan> {
            let (mut dag, base, tracker) = self.load(wt, input);
            let output = Output::new(&[]);
            plan(&mut dag, &base, &self.cache(wt), &output, &[], &tracker).unwrap()
        }

        fn destroy(&self, wt: &Worktree, input: &str) {
            let (mut dag, _base, _tracker) = self.load(wt, input);
            destroy(&mut dag, &wt.store, &Output::new(&[]), &[], false).unwrap();
        }

        fn files_under(&self, sub: &str) -> Vec<PathBuf> {
            fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
                let Ok(entries) = std::fs::read_dir(dir) else { return };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else {
                        out.push(path);
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.cache_dir.join(sub), &mut out);
            out
        }

        fn receipts(&self) -> Vec<PathBuf> {
            self.files_under("actions")
        }

        fn blobs(&self) -> Vec<PathBuf> {
            self.files_under("cas")
        }

        fn applies(&self) -> usize {
            self.applies.load(Ordering::SeqCst)
        }

        /// The single published receipt, parsed.
        fn only_receipt(&self) -> Receipt {
            let paths = self.receipts();
            assert_eq!(paths.len(), 1, "expected exactly one receipt");
            serde_json::from_slice(&std::fs::read(&paths[0]).unwrap()).unwrap()
        }
    }

    fn wrapped(store: &MemoryStore, block: &str) -> WrappedState {
        serde_json::from_value(store.load(block).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn restores_artifact_after_source_worktree_is_deleted() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let plans = h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 1);
        assert_eq!(h.receipts().len(), 1);
        assert_eq!(h.blobs().len(), 1);
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(plans[0].plan.reason.as_deref(), Some("cached"));
        assert_eq!(h.applies(), 1, "provider must not run on a cache hit");
        assert_eq!(std::fs::read_to_string(b.out()).unwrap(), "hello");
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(b.out()).unwrap().permissions());
        assert_eq!(mode & 0o111, 0o111);
        assert!(
            b.store.load("app").unwrap().is_some(),
            "restored state is saved locally"
        );

        // The restored block is now settled locally.
        let plans = h.apply(&b, &b.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(h.applies(), 1);
    }

    #[test]
    fn modifying_restored_output_cannot_corrupt_the_cache() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        h.apply(&b, &b.block("app", "file", ""), 1).unwrap();
        std::fs::write(b.out(), "tampered").unwrap();

        let c = h.worktree("c");
        c.write_src("hello");
        let plans = h.apply(&c, &c.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(std::fs::read_to_string(c.out()).unwrap(), "hello");
        assert_eq!(h.applies(), 1);
    }

    #[test]
    fn different_source_hashes_coexist_and_older_hash_hits() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        a.write_src("v2");
        let plans = h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Update);
        assert_eq!(h.applies(), 2);
        assert_eq!(h.receipts().len(), 2);

        a.write_src("v1");
        let plans = h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(h.applies(), 2);
        assert_eq!(std::fs::read_to_string(a.out()).unwrap(), "v1");
    }

    #[test]
    fn declared_outputs_reach_the_cache() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();

        assert_eq!(h.only_receipt().artifacts.keys().collect::<Vec<_>>(), ["out.txt"]);
    }

    /// An excluded output is still kept out of the action key -- it is a
    /// produced file, not a source -- but nothing about it is stored.
    #[test]
    fn uncached_outputs_are_not_stored() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        // Named with the trailing slash `output` uses for directories, which
        // is optional here.
        let block = a.block("app", "file", "  uncached = [\"out.txt/\"]\n");
        h.apply(&a, &block, 1).unwrap();

        assert!(h.only_receipt().artifacts.is_empty());

        // A second worktree still matches the action, because excluding an
        // output changes what is stored, not what the block depends on. It
        // therefore reuses the result without receiving the excluded file --
        // the cost of opting an output out.
        let b = h.worktree("b");
        b.write_src("v1");
        let plans = h
            .apply(&b, &b.block("app", "file", "  uncached = [\"out.txt/\"]\n"), 1)
            .unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(h.applies(), 1);
        assert!(!b.out().exists());
    }

    #[test]
    fn valid_receipt_is_bound_without_materializing() {
        let h = Harness::new();
        let a = h.worktree("a");
        h.apply(&a, &a.block("app", "file", "  content = \"hi\"\n"), 1).unwrap();

        // Reproduce what apply would have written, permissions included: a
        // receipt only counts as valid when the destination is byte- and
        // mode-identical to the captured artifact.
        let b = h.worktree("b");
        std::fs::write(b.out(), "hi").unwrap();
        std::fs::set_permissions(b.out(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let plans = h.apply(&b, &b.block("app", "file", "  content = \"hi\"\n"), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(plans[0].plan.reason.as_deref(), Some("cached"));
        assert_eq!(h.applies(), 1);
        assert!(b.store.load("app").unwrap().is_some());
    }

    #[test]
    fn plan_reports_restore_but_does_not_materialize() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.plan(&b, &b.block("app", "file", ""));
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert!(!b.out().exists());
        assert!(b.store.load("app").unwrap().is_none());
    }

    #[test]
    fn failed_tests_never_publish_receipts() {
        let h = Harness::new();
        let a = h.worktree("a");
        let result = h.apply(&a, &a.block("t", "check", "  content = \"x\"\n  passed = false\n"), 1);
        assert!(matches!(result, Err(EngineError::TestFailed { .. })));
        assert!(h.receipts().is_empty());
        assert!(a.store.load("t").unwrap().is_some(), "failure stays in local state");

        h.apply(&a, &a.block("t", "check", "  content = \"x\"\n  passed = true\n"), 1)
            .unwrap();
        assert_eq!(h.receipts().len(), 1);
    }

    #[test]
    fn corrupt_blob_is_a_miss() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        a.delete();
        for blob in h.blobs() {
            std::fs::set_permissions(&blob, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
            std::fs::write(&blob, "garbage").unwrap();
        }

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 2);
        assert_eq!(std::fs::read_to_string(b.out()).unwrap(), "hello");
        // The rebuilt artifact repopulates the cache.
        assert_eq!(h.blobs().len(), 1);
        assert_eq!(std::fs::read_to_string(&h.blobs()[0]).unwrap(), "hello");
    }

    #[test]
    fn corrupt_receipt_is_a_miss() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.block("app", "file", ""), 1).unwrap();
        a.delete();
        for receipt in h.receipts() {
            std::fs::write(&receipt, "{ nope").unwrap();
        }

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.block("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 2);
    }

    #[test]
    fn clean_does_not_delete_shared_receipts_or_blobs() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let input = a.block("app", "file", "");
        h.apply(&a, &input, 1).unwrap();
        h.destroy(&a, &input);
        assert!(!a.out().exists());
        assert!(a.store.load("app").unwrap().is_none());
        assert_eq!(h.receipts().len(), 1);
        assert_eq!(h.blobs().len(), 1);
    }

    #[test]
    fn parallel_dependents_use_the_result_selected_in_this_run() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        let b_dir = a.root.join("b");
        std::fs::create_dir_all(&b_dir).unwrap();
        let input = format!(
            "{}b = cached.file {{\n  dir = \"{}\"\n  content = \"from #{{a.path}}\"\n}}\n",
            a.block("a", "file", ""),
            b_dir.display()
        );

        h.apply(&a, &input, 4).unwrap();
        assert_eq!(
            wrapped(&a.store, "b").dep_hashes["a"],
            wrapped(&a.store, "a").content_hash
        );

        a.write_src("v2");
        let plans = h.apply(&a, &input, 4).unwrap();
        let by_name: HashMap<_, _> = plans.iter().map(|p| (p.name.as_str(), &p.plan)).collect();
        assert_eq!(by_name["a"].action, PlanAction::Update);
        assert_eq!(by_name["b"].action, PlanAction::Update);
        assert_eq!(by_name["b"].reason.as_deref(), Some("'a' changed"));
        assert_eq!(
            wrapped(&a.store, "b").dep_hashes["a"],
            wrapped(&a.store, "a").content_hash
        );
    }

    #[test]
    fn local_only_cache_never_shares() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let (mut dag, base, tracker) = h.load(&a, &a.block("app", "file", ""));
        let local = BuildCache::local_only(&a.root);
        apply(&mut dag, &base, &a.store, &local, &Output::new(&[]), &[], 1, &tracker).unwrap();
        assert!(h.receipts().is_empty());
        assert!(h.blobs().is_empty());
    }
}
