use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::cache::{ActionKey, ActionKeyInput, BuildCache, PortablePaths, PublishOutcome, RECEIPT_VERSION, Receipt};
use crate::dag::{
    self, AFTER_FIELD, CONCURRENCY_FIELD, DEPENDS_ON_FIELD, DIR_FIELD, Dag, DagError, DagNode, ENV_FIELD,
    UNCACHED_FIELD,
};
use crate::expr::{self, EvalError, Scope};
use crate::file_tracker::FileTracker;
use crate::loader::BaseScope;
use crate::output::{Event, Output, TargetWriter};
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, OutputFile, PlanAction, PlanResult, ReceiptCheck,
    ResourceKind, Target,
};
use crate::providers::is_ambient_key;
use crate::sha256::{Hasher, SHA256};
use crate::state::{StateError, StateStore};
use crate::value::{Map, Type, Value};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("{pos}: eval error in target '{target}': {source}")]
    Eval {
        pos: crate::ast::Pos,
        target: String,
        source: EvalError,
    },
    #[error("{pos}: target '{target}': {source}")]
    InvalidInputs {
        pos: crate::ast::Pos,
        target: String,
        source: Box<crate::input_check::InputError>,
    },
    #[error("{pos}: target '{target}' {phase} failed: {source}")]
    Provider {
        pos: crate::ast::Pos,
        target: String,
        phase: &'static str,
        source: BoxError,
    },
    #[error("{0}")]
    State(#[from] StateError),
    #[error("{pos}: protected target '{target}' cannot be {action}")]
    Protected {
        pos: crate::ast::Pos,
        target: String,
        action: &'static str,
    },
    #[error("{pos}: test target '{target}' failed")]
    TestFailed { pos: crate::ast::Pos, target: String },
    #[error("{pos}: target '{target}' concurrency must be a positive integer, got {value}")]
    InvalidConcurrency {
        pos: crate::ast::Pos,
        target: String,
        value: String,
    },
}

#[derive(Clone)]
struct ConcurrencyLimit {
    group: String,
    limit: usize,
}

/// Result of planning a single target.
pub struct TargetPlan {
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
    /// The target's `dir` when it last ran, which `--clean` runs in after the
    /// target definition has gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    /// The target's `env` when it last ran, for `--clean` likewise, less any
    /// secrets: state is stored in plain text.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    content_hash: SHA256,
    #[serde(default)]
    input_hash: SHA256,
    #[serde(default)]
    resolve_map: BTreeMap<String, SHA256>,
    #[serde(default)]
    dep_hashes: BTreeMap<String, SHA256>,
    /// The [`KEY_FORMAT`] `resolve_map` keys and `input_hash` were written in.
    #[serde(default)]
    key_format: u32,
}

/// Version of the path form in [`WrappedState`]. Format 0 wrote paths
/// relative to the project rather than the repository, so its keys name
/// different files in a nested project.
const KEY_FORMAT: u32 = 1;

/// Extracted prior state fields.
struct PriorState {
    provider_state: Option<serde_json::Value>,
    outputs: Map,
    dir: Option<String>,
    env: BTreeMap<String, String>,
    content_hash: SHA256,
    input_hash: SHA256,
    resolve_map: BTreeMap<String, SHA256>,
    dep_hashes: BTreeMap<String, SHA256>,
}

fn unwrap_state(stored: &serde_json::Value) -> PriorState {
    let wrapped: WrappedState =
        serde_json::from_value(stored.clone()).expect("corrupted state: not a valid WrappedState");
    // Resources recorded `dir` in their own state before the engine
    // managed it, so state written then still names it there.
    let dir = wrapped.dir.or_else(|| {
        let dir = wrapped.state.get(DIR_FIELD).and_then(serde_json::Value::as_str);
        dir.map(str::to_owned)
    });
    // Keys in an older format cannot be compared, and turning one back into
    // a path would name the wrong file. Dropping them reports every input
    // as changed, once.
    let (input_hash, resolve_map) = if wrapped.key_format == KEY_FORMAT {
        (wrapped.input_hash, wrapped.resolve_map)
    } else {
        (SHA256::ZERO, BTreeMap::new())
    };
    PriorState {
        provider_state: Some(wrapped.state),
        outputs: wrapped.outputs,
        dir,
        env: wrapped.env,
        content_hash: wrapped.content_hash,
        input_hash,
        resolve_map,
        dep_hashes: wrapped.dep_hashes,
    }
}

fn restore_prior(writer: &TargetWriter, prior_state: Option<&serde_json::Value>) -> PriorState {
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
        dir: None,
        env: BTreeMap::new(),
        content_hash: SHA256::ZERO,
        input_hash: SHA256::ZERO,
        resolve_map: BTreeMap::new(),
        dep_hashes: BTreeMap::new(),
    }
}

/// Hash the target's evaluated input fields in canonical (key-sorted) JSON
/// form, with absolute paths made portable so the hash agrees across
/// checkouts.
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

/// Result hash of every target completed so far in the current run: the exact
/// result that was selected, restored, or produced. Dependents key on these
/// rather than on persistent state so parallel execution is deterministic
/// and a target never observes a stale hash from a previous run.
type RunResults = HashMap<String, SHA256>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    Normal,
    Force,
}

/// Collect dependency hashes from the current run. A dependency outside the
/// run's order falls back to the state loaded when the DAG was built.
fn collect_dep_hashes(dag: &Dag, target_name: &str, results: &RunResults) -> BTreeMap<String, SHA256> {
    let mut dep_hashes = BTreeMap::new();
    let mut deps = dag.content_deps(target_name);
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

/// Describe why a target's content hash changed compared to its prior state.
fn change_reason(
    prior: &PriorState,
    new_resolve_map: &BTreeMap<String, SHA256>,
    new_input_hash: &SHA256,
    new_dep_hashes: &BTreeMap<String, SHA256>,
    dag: &Dag,
    target_name: &str,
    dirty_deps: &HashSet<String>,
) -> Option<String> {
    // Check for dirty dependency targets.
    let mut dirty: Vec<_> = dag
        .content_deps(target_name)
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

/// Validate that active targets don't reference missing (unresolved) params.
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
                    target: name.clone(),
                    source: crate::expr::EvalError::UndefinedVar(format!(
                        "missing required param '{r}' (use -P {r}=VALUE)"
                    )),
                });
            }
        }
    }
    Ok(())
}

/// Resolve the target execution order for the given selection.
/// - empty → `default` group if defined, else every non-`explicit` target
/// - `...` (alone or alongside other selections) → every non-`explicit` target,
///   unioned with the orders of any other named groups/targets (an `explicit`
///   target is included when named directly or pulled in as a dependency of
///   a selected group/target)
pub fn resolve_order(dag: &Dag, targets: &[String]) -> Result<Vec<String>, EngineError> {
    if targets.is_empty() {
        if dag.groups().contains_key("default") {
            return Ok(dag.selection_order("default")?);
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
        for name in dag.selection_order(t)? {
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

/// Narrow an already-resolved execution order to targets affected by a set of
/// changed paths. Directly affected targets pull in their content dependents
/// and every prerequisite required to execute the result.
pub fn affected_order(
    dag: &Dag,
    base: &BaseScope,
    cache: &BuildCache,
    order: &[String],
    changes: &crate::git::Changes,
    tracker: &Arc<FileTracker>,
) -> Result<Vec<String>, EngineError> {
    if order.is_empty() {
        return Ok(Vec::new());
    }

    let normalize = |path: &std::path::Path| cache.normalize_str(&path.to_string_lossy()).into_owned();
    let changed: HashSet<String> = changes.paths.iter().map(|path| normalize(path)).collect();
    let mut unmatched_removed: HashSet<String> = changes.removed.iter().map(|path| normalize(path)).collect();
    let visible: HashSet<String> = changes.visible.iter().map(|path| normalize(path)).collect();

    // Build definitions can change ownership, dependencies, or evaluated
    // inputs in ways that cannot be attributed from the new graph alone.
    if changes.paths.iter().any(|path| {
        path.file_name().is_some_and(|name| name == "BUILD.bit.lock")
            || path.extension().is_some_and(|extension| extension == "bit")
    }) {
        return Ok(order.to_vec());
    }

    tracker.reset();
    let mut scope = base.scope.clone();
    let mut affected = HashSet::new();

    for name in order {
        let node = dag
            .get_node(name)
            .ok_or_else(|| DagError::UnknownTarget(name.clone()))?;
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
                match resolve_sources(node, &inputs, tracker, cache.paths()) {
                    Ok(resolved) => {
                        current = cache.normalize_keys(&resolved.hashes);
                        current.extend(resolved.recorded_hashes);
                    }
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

        // The prior map tracks a directory output file by file, so a path
        // beneath a declared output is an output too.
        let is_output = |path: &str| {
            outputs.iter().any(|output| {
                path == output
                    || path
                        .strip_prefix(output.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };

        // Git cannot report a changed tool version or environment variable,
        // or a change to a file it does not see, such as an ignored file or
        // one outside the repository. Those are compared with the prior run
        // instead. Only the prior map holds output hashes, so an output that
        // is not also a source is left out rather than always differing. As
        // with the inputs above, a target that never ran has nothing to
        // compare against.
        if prior.provider_state.is_some() {
            let unseen_keys = current
                .keys()
                .chain(prior.resolve_map.keys())
                .filter(|key| is_ambient_key(key) || !visible.contains(key.as_str()))
                .filter(|key| current.contains_key(*key) || !is_output(key));
            for key in unseen_keys {
                if current.get(key) != prior.resolve_map.get(key) {
                    direct = true;
                }
            }
        }

        let tracked: HashSet<&str> = current
            .keys()
            .chain(prior.resolve_map.keys())
            .filter(|key| !is_ambient_key(key))
            .chain(outputs.iter())
            .map(String::as_str)
            .collect();
        for path in &tracked {
            if changed.contains(*path) {
                direct = true;
            }
            unmatched_removed.remove(*path);
        }

        // A target with no source paths cannot be proven unaffected by Git.
        if tracked.iter().all(|path| is_output(path)) {
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
        for dependency in dag.selection_order(&name)? {
            needed.insert(dependency);
        }
    }
    Ok(order.iter().filter(|name| needed.contains(*name)).cloned().collect())
}

/// A usable shared-cache receipt found for a target.
struct CacheHit {
    receipt: Receipt,
    check: ReceiptCheck,
}

/// Everything decided about a target before any side effect. Shared by plan
/// and apply so both report the same action.
struct Prepared {
    inputs: Map,
    prior: PriorState,
    /// Normalized source hashes returned by the provider.
    sources: BTreeMap<String, SHA256>,
    input_hash: SHA256,
    dep_hashes: BTreeMap<String, SHA256>,
    content_hash: SHA256,
    changes: Changes,
    plan: PlanResult,
    /// Declared outputs paired with their portable roles. Empty for
    /// resources the shared cache never consults.
    outputs: Vec<OutputFile>,
    /// The subset of `outputs` the shared cache stores and restores. Both an
    /// excluded output and a cached one are kept out of the action key, since
    /// neither is a source; only this subset reaches the CAS.
    cached_outputs: Vec<OutputFile>,
    /// Whether the shared cache was consulted, so a successful result may
    /// be published to it.
    shared: bool,
    hit: Option<CacheHit>,
}

impl Prepared {
    /// Hash a dependent should use for this target if it is not going to run.
    fn settled_hash(&self) -> Option<SHA256> {
        match (&self.plan.action, &self.hit) {
            (PlanAction::None, Some(_)) => Some(self.content_hash),
            (PlanAction::None, None) => self.prior.provider_state.is_some().then_some(self.prior.content_hash),
            (PlanAction::Restore, Some(hit)) => Some(hit.receipt.content_hash),
            _ => None,
        }
    }
}

/// Pair each key a resource declares as an output with its portable form.
/// The portable form is what a receipt records, so that a receipt written in
/// one checkout is legible in every other; the key itself stays as
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

/// Keep source provenance for the shared key while tracking output drift in
/// the local content hash. An output that is also a source remains a source.
#[derive(Default)]
struct ResolvedHashes {
    sources: BTreeMap<String, SHA256>,
    tracked: BTreeMap<String, SHA256>,
    /// Names of the variables the tool recorded reading. Their values are in
    /// `tracked` but not `sources`; see [`Ambient::RecordedVariable`].
    recorded: Vec<String>,
}

/// A target's sources as [`resolve_sources`] finds them.
struct Sources {
    /// Files and ambient fingerprints, all known before the action runs.
    hashes: BTreeMap<String, SHA256>,
    /// Variables the tool recorded reading: by name, and their fingerprints
    /// keyed as [`crate::providers::hash_ambient`] keys any variable.
    recorded: Vec<String>,
    recorded_hashes: BTreeMap<String, SHA256>,
}

/// Whether every variable `receipt` recorded still has the value it had
/// when the receipt was published.
fn recorded_variables_match(receipt: &Receipt, paths: &PortablePaths) -> bool {
    receipt
        .variables
        .iter()
        .all(|(name, fingerprint)| crate::providers::variable_fingerprint(name, paths) == *fingerprint)
}

/// The target's sources: the files its resource resolves and the ambient
/// state it depends on, so a tool upgrade or changed variable is a source
/// change like any other.
fn resolve_sources(
    node: &DagNode,
    inputs: &Map,
    tracker: &FileTracker,
    paths: &PortablePaths,
) -> Result<Sources, BoxError> {
    // Resources lock the tracker themselves while resolving.
    let start = std::time::Instant::now();
    let mut hashes = node.resource.resolve(inputs)?;
    crate::output::debug_current(|| {
        format!(
            "resolved {} input(s) in {}ms",
            hashes.len(),
            start.elapsed().as_millis()
        )
    });
    let start = std::time::Instant::now();
    let mut ambient = Vec::new();
    let mut recorded = Vec::new();
    let env = crate::provider::target_env(inputs);
    for item in node.resource.ambient(inputs)? {
        match item {
            Ambient::RecordedVariable(name) => recorded.push(name),
            Ambient::Version(mut command) => {
                command.target_env.clone_from(&env);
                ambient.push(Ambient::Version(command));
            }
            other => ambient.push(other),
        }
    }
    let fingerprints = crate::providers::hash_ambient(&ambient, tracker, paths)?;
    crate::output::debug_current(|| {
        format!(
            "fingerprinted {} tool(s) and {} recorded variable(s) in {}ms",
            fingerprints.len(),
            recorded.len(),
            start.elapsed().as_millis()
        )
    });
    hashes.extend(fingerprints);
    let as_variables: Vec<Ambient> = recorded.iter().cloned().map(Ambient::Variable).collect();
    let recorded_hashes = crate::providers::hash_ambient(&as_variables, tracker, paths)?;
    Ok(Sources {
        hashes,
        recorded,
        recorded_hashes,
    })
}

fn resolve_hashes(
    node: &DagNode,
    inputs: &Map,
    outputs: &[OutputFile],
    cache: &BuildCache,
    tracker: &FileTracker,
) -> Result<ResolvedHashes, BoxError> {
    let resolved = resolve_sources(node, inputs, tracker, cache.paths())?;
    let sources = cache.normalize_keys(&resolved.hashes);
    let mut tracked = sources.clone();
    tracked.extend(resolved.recorded_hashes);
    for output in outputs {
        let path = Path::new(&output.path);
        if path.is_file() {
            tracked.insert(output.role.clone(), tracker.hash_file(path)?);
        } else if path.is_dir() {
            // Each file is tracked under its own key, so deleting or editing
            // one reruns the target and is reported as a changed path.
            for file in files_under(path)? {
                let key = cache.normalize_str(&file.to_string_lossy()).into_owned();
                tracked.insert(key, tracker.hash_file(&file)?);
            }
        }
    }
    Ok(ResolvedHashes {
        sources,
        tracked,
        recorded: resolved.recorded,
    })
}

/// Every file beneath `dir`. Symlinked directories are not followed, so a
/// link cycle cannot recurse forever.
fn files_under(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            files.extend(files_under(&path)?);
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

/// The target's `dir` field.
fn target_dir(inputs: &Map) -> Option<&str> {
    inputs.get(DIR_FIELD).and_then(Value::as_str)
}

/// The target's `env` as [`WrappedState`] records it: without secrets, whose
/// values must not reach the state file.
fn recorded_env(inputs: &Map) -> BTreeMap<String, String> {
    let Some(env) = inputs.get(ENV_FIELD).and_then(Value::as_map) else {
        return BTreeMap::new();
    };
    env.iter()
        .filter(|(_, value)| !value.contains_secret())
        .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
        .collect()
}

/// The entries of `uncached`, relative to `dir` like the outputs they name.
/// A trailing slash marks a directory in `outputs`, so it is optional here
/// too and matching ignores it.
fn uncached_outputs(inputs: &Map) -> HashSet<String> {
    let entries: Vec<&str> = match inputs.get(UNCACHED_FIELD) {
        Some(Value::List(_, items)) => items.iter().filter_map(|v| v.as_str()).collect(),
        Some(Value::Str(one)) => vec![one.as_str()],
        _ => vec![],
    };
    let dir = target_dir(inputs);
    entries
        .into_iter()
        .map(|entry| crate::provider::in_dir(dir, entry).trim_end_matches('/').to_owned())
        .collect()
}

/// Whether `uncached` names this output, under either of its spellings.
fn is_uncached(uncached: &HashSet<String>, output: &OutputFile) -> bool {
    uncached.contains(output.path.trim_end_matches('/')) || uncached.contains(output.role.trim_end_matches('/'))
}

/// Compute the shared action key from provider-resolved source hashes,
/// which include the ambient fingerprints. Sources are measured before apply
/// for lookup and after apply for publication.
fn action_key(
    name: &str,
    node: &DagNode,
    prepared: &Prepared,
    sources: &BTreeMap<String, SHA256>,
    cache: &BuildCache,
) -> Result<ActionKey, BoxError> {
    let CachePolicy::Shared { version } = node.resource.cache_policy(&prepared.inputs) else {
        return Err("resource is not shared".into());
    };
    Ok(ActionKeyInput {
        receipt_version: RECEIPT_VERSION,
        provider: &node.provider,
        resource: &node.resource_name,
        cache_version: version,
        target: name,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        inputs: hash_inputs(&prepared.inputs, cache),
        sources,
        deps: &prepared.dep_hashes,
    }
    .key())
}

fn provider_error(node: &DagNode, name: &str, phase: &'static str) -> impl FnOnce(BoxError) -> EngineError {
    let pos = node.pos.clone();
    let target = name.to_owned();
    move |source| EngineError::Provider {
        pos,
        target,
        phase,
        source,
    }
}

/// What changed since the baseline the prior state records, which a failed
/// test run leaves at the last success.
fn changes_since_success(
    prior: &PriorState,
    input_hash: &SHA256,
    tracked: &BTreeMap<String, SHA256>,
    dep_hashes: &BTreeMap<String, SHA256>,
    cache: &BuildCache,
    mode: ApplyMode,
) -> Changes {
    if mode == ApplyMode::Force
        || prior.provider_state.is_none()
        || *input_hash != prior.input_hash
        || *dep_hashes != prior.dep_hashes
    {
        return Changes::All;
    }
    let mut paths = BTreeSet::new();
    for key in tracked.keys().chain(prior.resolve_map.keys()) {
        if tracked.get(key) == prior.resolve_map.get(key) {
            continue;
        }
        if is_ambient_key(key) {
            return Changes::All;
        }
        paths.insert(cache.absolute(key));
    }
    Changes::Paths(paths)
}

/// Resolve inputs, compute hashes, plan, and consult the shared cache.
#[allow(clippy::too_many_arguments)]
fn prepare_target(
    name: &str,
    node: &DagNode,
    dag: &Dag,
    inputs: Map,
    dep_hashes: BTreeMap<String, SHA256>,
    dirty: &HashSet<String>,
    cache: &BuildCache,
    tracker: &FileTracker,
    writer: &TargetWriter,
    mode: ApplyMode,
) -> Result<Prepared, EngineError> {
    let _scope = crate::output::TargetScope::enter(writer);
    let prior = restore_prior(writer, node.prior_state.as_ref());

    let outputs = output_files(node, &inputs, cache).map_err(provider_error(node, name, "resolve"))?;
    let ResolvedHashes {
        sources,
        tracked: resolve_map,
        ..
    } = resolve_hashes(node, &inputs, &outputs, cache, tracker).map_err(provider_error(node, name, "resolve"))?;
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
    let changes = changes_since_success(&prior, &input_hash, &resolve_map, &dep_hashes, cache, mode);

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

    if mode == ApplyMode::Force && plan.action == PlanAction::None {
        plan.action = if prior.provider_state.is_some() {
            PlanAction::Update
        } else {
            PlanAction::Create
        };
        plan.reason = Some("forced".into());
    }

    if node.protected && plan.action == PlanAction::Destroy {
        return Err(EngineError::Protected {
            pos: node.pos.clone(),
            target: name.to_owned(),
            action: "destroyed",
        });
    }

    let mut prepared = Prepared {
        inputs,
        prior,
        sources,
        input_hash,
        dep_hashes,
        content_hash,
        changes,
        plan,
        outputs,
        cached_outputs: Vec::new(),
        shared: false,
        hit: None,
    };

    if mode == ApplyMode::Force
        || node.resource.cache_policy(&prepared.inputs) == CachePolicy::Local
        || !cache.is_shared()
        || prepared.plan.action == PlanAction::None
    {
        return Ok(prepared);
    }
    let uncached = uncached_outputs(&prepared.inputs);
    prepared.cached_outputs = prepared
        .outputs
        .iter()
        .filter(|o| !is_uncached(&uncached, o))
        .cloned()
        .collect();
    let key =
        action_key(name, node, &prepared, &prepared.sources, cache).map_err(provider_error(node, name, "resolve"))?;
    prepared.shared = true;

    // Look up a receipt only when every dependency hash is settled. A
    // previously failed test always reruns locally so the failure is
    // observed here rather than masked by another worktree's success.
    if previously_failed || has_dirty_dep {
        return Ok(prepared);
    }
    let receipt = match cache.lookup(&key, |receipt| recorded_variables_match(receipt, cache.paths())) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            crate::debug!(writer, "cache miss (key={})", short_hash(&key.digest()));
            return Ok(prepared);
        }
        Err(e) => {
            report_cache_error(writer, cache, "cache lookup failed", &e);
            return Ok(prepared);
        }
    };
    let Some(cas) = cache.cas() else {
        return Ok(prepared);
    };
    if let Err(e) = cache.fetch(receipt.artifacts.values(), false) {
        report_cache_error(writer, cache, "cannot fetch the cached result", &e);
        return Ok(prepared);
    }
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

/// Plan all targets in the DAG (or a selected subset), returning what would
/// change. Consults the shared cache read-only: a restorable receipt is
/// reported as [`PlanAction::Restore`] but nothing is materialized.
pub fn plan(
    dag: &mut Dag,
    base: &BaseScope,
    cache: &BuildCache,
    output: &Output,
    targets: &[String],
    tracker: &Arc<FileTracker>,
) -> Result<Vec<TargetPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    plan_selected(dag, base, cache, output, &order, tracker)
}

/// Plan an exact, already-resolved target order.
pub fn plan_selected(
    dag: &mut Dag,
    base: &BaseScope,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    tracker: &Arc<FileTracker>,
) -> Result<Vec<TargetPlan>, EngineError> {
    tracker.reset();
    validate_active_params(dag, order, base)?;
    crate::debug!(output, "plan: {} target(s) in order: {}", order.len(), order.join(", "));

    let mut scope = base.scope.clone();
    let mut plans = Vec::new();
    let mut dirty: HashSet<String> = HashSet::new();
    let mut results = RunResults::new();

    for name in order {
        let node = dag
            .get_node(name)
            .ok_or_else(|| DagError::UnknownTarget(name.clone()))?;
        let writer = output.writer_indented(name, 0);
        evaluate_concurrency(node, &scope)?;

        let inputs = eval_fields_lenient(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            target: name.clone(),
            source: e,
        })?;
        let dep_hashes = collect_dep_hashes(dag, name, &results);
        let prepared = prepare_target(
            name,
            node,
            dag,
            inputs,
            dep_hashes,
            &dirty,
            cache,
            tracker,
            &writer,
            ApplyMode::Normal,
        )?;

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
        writer.event_with_reason(event, &prepared.plan.description, prepared.plan.reason.as_deref());

        let outputs = match prepared.hit {
            Some(hit) => hit.receipt.outputs,
            None => prepared.prior.outputs,
        };
        scope.set(name, Value::strct(outputs));

        plans.push(TargetPlan {
            name: name.clone(),
            plan: prepared.plan,
        });
    }

    Ok(plans)
}

/// Apply all targets in the DAG (or a selected subset).
/// With no selection: runs the `default` group if defined, else all targets.
/// With `["..."]`: runs all targets.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    targets: &[String],
    jobs: usize,
    tracker: &Arc<FileTracker>,
) -> Result<Vec<TargetPlan>, EngineError> {
    let order = resolve_order(dag, targets)?;
    apply_selected(dag, base, store, cache, output, &order, jobs, tracker)
}

/// Apply an exact, already-resolved target order.
#[allow(clippy::too_many_arguments)]
pub fn apply_selected(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<FileTracker>,
) -> Result<Vec<TargetPlan>, EngineError> {
    apply_selected_with_mode(dag, base, store, cache, output, order, jobs, tracker, ApplyMode::Normal)
}

/// Force-apply an exact, already-resolved target order without consulting the
/// shared cache or skipping unchanged targets.
#[allow(clippy::too_many_arguments)]
pub fn apply_selected_forced(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<FileTracker>,
) -> Result<Vec<TargetPlan>, EngineError> {
    apply_selected_with_mode(dag, base, store, cache, output, order, jobs, tracker, ApplyMode::Force)
}

#[allow(clippy::too_many_arguments)]
fn apply_selected_with_mode(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<FileTracker>,
    mode: ApplyMode,
) -> Result<Vec<TargetPlan>, EngineError> {
    tracker.reset();
    validate_active_params(dag, order, base)?;
    crate::debug!(
        output,
        "apply: {} target(s), jobs={}, order: {}",
        order.len(),
        jobs,
        order.join(", ")
    );
    let concurrency = concurrency_limits(dag, &base.scope, order)?;
    if jobs <= 1 {
        apply_order(dag, base, store, cache, output, order, tracker, mode)
    } else {
        apply_order_parallel(
            dag,
            base,
            store,
            cache,
            output,
            order,
            jobs,
            tracker,
            &concurrency,
            mode,
        )
    }
}

/// Apply targets sequentially in the given order.
#[allow(clippy::too_many_arguments)]
fn apply_order(
    dag: &mut Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    tracker: &Arc<FileTracker>,
    mode: ApplyMode,
) -> Result<Vec<TargetPlan>, EngineError> {
    let mut scope = base.scope.clone();
    let mut results = RunResults::new();
    let mut plans = Vec::new();

    for name in order {
        let node = dag
            .get_node(name)
            .ok_or_else(|| DagError::UnknownTarget(name.clone()))?;
        let writer = output.writer(name);
        let dep_hashes = collect_dep_hashes(dag, name, &results);
        let result = execute_target(name, node, dag, &scope, dep_hashes, cache, &writer, tracker, mode)?;
        let test_failed = result.test_failed;
        let pos = result.pos.clone();
        complete_target(result, store, output, &mut scope, &mut results, &mut plans)?;
        if test_failed {
            return Err(EngineError::TestFailed {
                pos,
                target: name.clone(),
            });
        }
    }

    Ok(plans)
}

/// Result sent back from a worker thread after executing a target.
struct TargetResult {
    pos: crate::ast::Pos,
    name: String,
    plan: PlanResult,
    outputs: Map,
    /// The wrapped state to persist, if the target was applied or restored.
    wrapped_state: Option<serde_json::Value>,
    /// Hash dependents should use for this target, if it has a settled result.
    result_hash: Option<SHA256>,
    /// Whether this was a failed test (passed == false).
    test_failed: bool,
}

/// Persist a completed target's state and make its result visible to
/// dependents.
fn complete_target(
    result: TargetResult,
    store: &dyn StateStore,
    output: &Output,
    scope: &mut Scope,
    results: &mut RunResults,
    plans: &mut Vec<TargetPlan>,
) -> Result<(), EngineError> {
    if let Some(state) = &result.wrapped_state {
        store.save(&result.name, state)?;
        crate::debug!(output.writer(&result.name), "saved state");
    }
    if let Some(hash) = result.result_hash {
        results.insert(result.name.clone(), hash);
    }
    scope.set(&result.name, Value::strct(result.outputs));
    plans.push(TargetPlan {
        name: result.name,
        plan: result.plan,
    });
    Ok(())
}

/// Apply targets in parallel using a ready-queue scheduler.
#[allow(clippy::too_many_arguments)]
fn apply_order_parallel(
    dag: &Dag,
    base: &BaseScope,
    store: &dyn StateStore,
    cache: &BuildCache,
    output: &Output,
    order: &[String],
    jobs: usize,
    tracker: &Arc<FileTracker>,
    concurrency: &HashMap<String, Option<ConcurrencyLimit>>,
    mode: ApplyMode,
) -> Result<Vec<TargetPlan>, EngineError> {
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
    let mut plans: Vec<TargetPlan> = Vec::new();
    let mut completed = 0;
    let total = order.len();

    std::thread::scope(|s| {
        let (result_tx, result_rx) = mpsc::channel::<(String, Result<TargetResult, EngineError>)>();
        let mut in_flight = 0;
        let mut running_by_group: HashMap<String, usize> = HashMap::new();
        let mut failed: Option<EngineError> = None;

        loop {
            // Dispatch ready targets up to the job limit. Dependency hashes are
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
                let node = dag.get_node(&name).expect("target in order");
                let writer = output.writer(&name);
                let scope_snapshot = scope.clone();
                let dep_hashes = collect_dep_hashes(dag, &name, &results);
                let tx = result_tx.clone();
                let completed_name = name.clone();

                if let Some(config) = &concurrency[&name] {
                    *running_by_group.entry(config.group.clone()).or_default() += 1;
                }

                s.spawn(move || {
                    let result = execute_target(
                        &name,
                        node,
                        dag,
                        &scope_snapshot,
                        dep_hashes,
                        cache,
                        &writer,
                        tracker,
                        mode,
                    );
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
                Ok(target_result) => {
                    let name = target_result.name.clone();
                    let test_failed = target_result.test_failed;
                    let pos = target_result.pos.clone();

                    if let Err(e) = complete_target(target_result, store, output, &mut scope, &mut results, &mut plans)
                    {
                        failed = Some(e);
                        continue;
                    }

                    if test_failed {
                        failed = Some(EngineError::TestFailed {
                            pos,
                            target: name.clone(),
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
    writer: &TargetWriter,
) -> Result<ApplyResult<serde_json::Value, Map>, BoxError> {
    let cas = cache.cas().ok_or("shared cache unavailable")?;
    // A valid receipt's outputs are already in place, so only a restore
    // needs the artifacts' contents.
    if hit.check == ReceiptCheck::Restore {
        cache.fetch(hit.receipt.artifacts.values(), true)?;
    }
    let materialize = || {
        node.resource
            .materialize(inputs, &hit.receipt.state, outputs, &hit.receipt.artifacts, cas, writer)
    };
    // Restoring declared outputs moves temporary files into place while other
    // targets may be resolving inputs. The tracker skips discovered files that
    // vanish before they are hashed, and rehashes restored files whose
    // metadata changed, so this needs no coordination with resolution.
    match materialize()? {
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
    post_sources: &BTreeMap<String, SHA256>,
    recorded: &[String],
    post_hash: SHA256,
    state: &serde_json::Value,
    outputs: &Map,
    cache: &BuildCache,
    writer: &TargetWriter,
) {
    let CachePolicy::Shared { version } = node.resource.cache_policy(&prepared.inputs) else {
        return;
    };
    let Some(cas) = cache.cas() else {
        return;
    };
    let attempt = || -> Result<(ActionKey, Receipt, PublishOutcome), BoxError> {
        let artifacts = node
            .resource
            .capture_artifacts(&prepared.inputs, state, &prepared.cached_outputs, cas)?;
        let key = action_key(name, node, prepared, post_sources, cache)?;
        let receipt = Receipt {
            version: RECEIPT_VERSION,
            provider: node.provider.clone(),
            resource: node.resource_name.clone(),
            cache_version: version,
            state: state.clone(),
            outputs: outputs.clone(),
            artifacts,
            content_hash: post_hash,
            variables: recorded
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        crate::providers::variable_fingerprint(name, cache.paths()),
                    )
                })
                .collect(),
        };
        let outcome = cache.publish(&key, &receipt)?;
        Ok((key, receipt, outcome))
    };
    let (key, receipt) = match attempt() {
        Ok((key, _, PublishOutcome::Conflict)) => return warn_conflict(writer, &key),
        Ok((key, receipt, outcome)) => {
            if outcome == PublishOutcome::Published {
                crate::debug!(writer, "published receipt (key={})", short_hash(&key.digest()));
            }
            (key, receipt)
        }
        Err(e) => return writer.stderr_line(&format!("warning: could not publish to the shared cache: {e}")),
    };
    match cache.upload(&key, &receipt) {
        Ok(Some(PublishOutcome::Conflict)) => warn_conflict(writer, &key),
        Ok(Some(_)) => crate::debug!(writer, "uploaded receipt (key={})", short_hash(&key.digest())),
        Ok(None) => {}
        Err(e) if e.is_remote() => report_cache_error(writer, cache, "could not publish to the remote cache", &e),
        Err(e) => writer.stderr_line(&format!("warning: could not publish to the remote cache: {e}")),
    }
}

/// Report a failure of the remote cache as a warning the first time in a
/// run, since every later target would only repeat it, and anything else in
/// the debug log. A miss is the outcome either way, so the run continues.
fn report_cache_error(writer: &TargetWriter, cache: &BuildCache, context: &str, error: &crate::cache::CacheError) {
    if cache.first_remote_failure(error) {
        // A message, unlike an output line, stays visible when the target
        // succeeds, as it will: the target just runs instead.
        writer.message(&crate::output::Message::new(
            crate::output::MessageLevel::Warning,
            format!("{context}: {error}; further remote cache failures this run are not shown"),
        ));
    } else {
        crate::debug!(writer, "{context}: {error}");
    }
}

fn warn_conflict(writer: &TargetWriter, key: &ActionKey) {
    writer.stderr_line(&format!(
        "warning: a different receipt already exists for this action (key={}); keeping it. \
         The action key may be missing an input the result depends on.",
        short_hash(&key.digest())
    ));
}

/// Execute a single target: evaluate fields, plan, restore or apply if
/// needed, compute the post-apply hash, and publish a receipt.
#[allow(clippy::too_many_arguments)]
fn execute_target(
    name: &str,
    node: &DagNode,
    dag: &Dag,
    scope: &Scope,
    dep_hashes: BTreeMap<String, SHA256>,
    cache: &BuildCache,
    writer: &TargetWriter,
    tracker: &FileTracker,
    mode: ApplyMode,
) -> Result<TargetResult, EngineError> {
    let _scope = crate::output::TargetScope::enter(writer);
    let inputs = eval_fields(&node.fields, scope).map_err(|e| EngineError::Eval {
        pos: node.pos.clone(),
        target: name.to_owned(),
        source: e,
    })?;
    // The loader could not see values that come from upstream outputs.
    if !crate::module::is_instance(node) {
        crate::input_check::check_values(&inputs, &node.resource.schema().inputs).map_err(|source| {
            EngineError::InvalidInputs {
                pos: node.pos.clone(),
                target: name.to_owned(),
                source: Box::new(source),
            }
        })?;
    }

    let mut prepared = prepare_target(
        name,
        node,
        dag,
        inputs,
        dep_hashes,
        &HashSet::new(),
        cache,
        tracker,
        writer,
        mode,
    )?;
    crate::debug!(writer, "planned action: {:?}", prepared.plan.action);

    // Bind or restore from a shared receipt. Any failure here is a cache
    // miss: fall through to running the provider.
    let mut bound: Option<ApplyResult<serde_json::Value, Map>> = None;
    if let Some(hit) = &prepared.hit {
        if hit.check == ReceiptCheck::Restore {
            writer.event_with_reason(
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
        return Ok(TargetResult {
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
            let reason = prepared.plan.reason.as_deref();
            let announcing;
            let apply_writer = if node.resource.announces_start() {
                announcing = writer.with_start_reason(reason);
                &announcing
            } else {
                writer.event_with_reason(Event::Starting, &prepared.plan.description, reason);
                writer
            };
            node.resource
                .apply(
                    &prepared.inputs,
                    prepared.prior.provider_state.as_ref(),
                    &prepared.changes,
                    apply_writer,
                )
                .map_err(|e| {
                    writer.event(Event::Failed, &e.to_string());
                    EngineError::Provider {
                        pos: node.pos.clone(),
                        target: name.to_owned(),
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
        // such as formatters change their own inputs. The tracker rehashes
        // any file whose metadata changed, but globs and providers' parsed
        // sources may predate files the target added, here and for every
        // target that resolves after it.
        tracker.invalidate();
        let ResolvedHashes {
            sources: post_sources,
            tracked: post_resolve,
            recorded,
        } = resolve_hashes(node, &prepared.inputs, &prepared.outputs, cache, tracker).unwrap_or_default();
        let post_hash = compute_content_hash(&prepared.input_hash, &post_resolve, &prepared.dep_hashes);
        crate::debug!(writer, "post-apply hash={}", short_hash(&post_hash));
        result_hash = Some(post_hash);

        // Failed tests stay in local state so they rerun, but never enter
        // the shared cache.
        if !from_cache && !test_failed && prepared.shared {
            publish_receipt(
                name,
                node,
                &prepared,
                &post_sources,
                &recorded,
                post_hash,
                new_state,
                &apply_result.outputs,
                cache,
                writer,
            );
        }

        // A failed test run keeps the last success as the baseline, so the
        // next run's changes still include those this run tested.
        let (input_hash, resolve_map, dep_hashes) = if test_failed {
            (
                prepared.prior.input_hash,
                prepared.prior.resolve_map,
                prepared.prior.dep_hashes,
            )
        } else {
            (prepared.input_hash, post_resolve, prepared.dep_hashes)
        };
        let wrapped = WrappedState {
            state: new_state.clone(),
            outputs: apply_result.outputs.clone(),
            dir: target_dir(&prepared.inputs).map(str::to_owned),
            env: recorded_env(&prepared.inputs),
            content_hash: post_hash,
            input_hash,
            resolve_map,
            dep_hashes,
            key_format: KEY_FORMAT,
        };
        Some(serde_json::to_value(&wrapped).expect("serialize wrapped state"))
    } else {
        crate::debug!(writer, "apply returned no state (stateless target)");
        None
    };

    if test_failed {
        writer.event(Event::Failed, "tests failed");
    } else if from_cache && prepared.plan.action == PlanAction::None {
        writer.event(Event::Skipped, "cached");
    } else {
        writer.event(Event::Ok, "");
    }

    Ok(TargetResult {
        pos: node.pos.clone(),
        name: name.to_owned(),
        plan: prepared.plan,
        outputs: apply_result.outputs,
        wrapped_state,
        result_hash,
        test_failed,
    })
}

/// Destroy each selected target and all of its transitive dependents, in reverse
/// topological order (sinks first, so each target is torn down before the
/// targets it depends on).
///
/// # Arguments
///
/// * `targets` - Target or group names to destroy. An empty slice, or any
///   list containing the literal `"..."`, destroys every non-`explicit`
///   target in the DAG (unioned with the transitive dependents of the other
///   named groups and targets); `explicit` targets must be named to be destroyed.
///   Protected targets are destroyed only when their exact target name appears
///   in this list. Selecting them through a group or `...` is not sufficient.
/// * `continue_on_error` - When true, continue past provider errors (the
///   failing target still shows an error, but remaining targets proceed).
///
/// # Errors
///
/// Returns [`EngineError`] if a selected name is unknown, a cycle exists, or a
/// provider fails during teardown (unless `continue_on_error` is set, in
/// which case provider errors are collected and the first is returned after
/// all targets have been processed).
pub fn destroy(
    dag: &mut Dag,
    store: &dyn StateStore,
    output: &Output,
    targets: &[String],
    continue_on_error: bool,
) -> Result<(), EngineError> {
    let mut order: Vec<String> = if targets.is_empty() {
        dag.select_all()?
    } else {
        // Gather each selection's transitive dependents (inclusive) and emit them
        // in a single topological order so tie-breaking stays deterministic.
        // `...` is composable: it contributes every non-`explicit` target,
        // unioned with the dependents of any other named groups and targets.
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
        "destroy: {} target(s) in reverse order: {}",
        order.len(),
        order.join(", ")
    );

    let mut first_error: Option<EngineError> = None;

    for name in &order {
        let node = dag
            .get_node(name)
            .ok_or_else(|| DagError::UnknownTarget(name.clone()))?;
        let writer = output.writer(name);
        let _scope = crate::output::TargetScope::enter(&writer);

        if node.protected && !targets.iter().any(|target| target == name) {
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

        let prior_state = Target::new(prior.dir, provider_state).with_env(prior.env);
        if let Err(e) = node.resource.destroy(&prior_state, &writer) {
            let err = EngineError::Provider {
                pos: node.pos.clone(),
                target: name.clone(),
                phase: "destroy",
                source: e,
            };
            writer.event(Event::Failed, &format!("{err}"));
            if !continue_on_error {
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

/// Dump evaluated inputs and stored outputs for all targets (or a selected subset).
pub fn dump(dag: &mut Dag, base: &BaseScope, targets: &[String]) -> Result<(), EngineError> {
    let order = resolve_order(dag, targets)?;
    dump_selected(dag, base, &order, false)
}

/// Dump evaluated inputs and stored outputs for an exact target order.
pub fn dump_selected(dag: &mut Dag, base: &BaseScope, order: &[String], quiet: bool) -> Result<(), EngineError> {
    let mut scope = base.scope.clone();

    for (i, name) in order.iter().enumerate() {
        let node = dag
            .get_node(name)
            .ok_or_else(|| DagError::UnknownTarget(name.clone()))?;

        // Evaluate inputs, but replace depends_on/after with target names
        let mut inputs = eval_fields_lenient(&node.fields, &scope).map_err(|e| EngineError::Eval {
            pos: node.pos.clone(),
            target: name.clone(),
            source: e,
        })?;
        let depends_on = dag::collect_dependency_refs(&node.fields, DEPENDS_ON_FIELD, &scope).map_err(|source| {
            EngineError::Eval {
                pos: node.pos.clone(),
                target: name.clone(),
                source,
            }
        })?;
        if !depends_on.is_empty() {
            inputs.insert(
                DEPENDS_ON_FIELD.into(),
                Value::List(Type::String, depends_on.into_iter().map(Value::Str).collect()),
            );
        }
        let after =
            dag::collect_dependency_refs(&node.fields, AFTER_FIELD, &scope).map_err(|source| EngineError::Eval {
                pos: node.pos.clone(),
                target: name.clone(),
                source,
            })?;
        if !after.is_empty() {
            inputs.insert(
                AFTER_FIELD.into(),
                Value::List(Type::String, after.into_iter().map(Value::Str).collect()),
            );
        }

        let prior = match &node.prior_state {
            Some(s) => unwrap_state(s),
            None => default_prior(),
        };

        // Populate scope with stored outputs for downstream refs
        scope.set(name, Value::strct(prior.outputs.clone()));

        if quiet {
            continue;
        }

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
                println!("{pad}  - {}", dump_scalar(item));
            }
        }
        _ => println!("{pad}{key}: {}", dump_scalar(value)),
    }
}

/// Show a secret as the `secret("KEY")` call that reads it, never its value.
fn dump_scalar(value: &Value) -> String {
    match value {
        Value::Secret { .. } => value.to_literal(),
        _ => value.to_string(),
    }
}

/// Fields left out of a target's evaluated inputs. `after` must stay out
/// because the inputs feed the content hash, and evaluating `after = [a]`
/// yields `a`'s outputs, which would couple the target to `a`'s content. The
/// DAG takes `after` edges from the unevaluated fields instead.
fn is_unhashed_field(name: &str) -> bool {
    name == CONCURRENCY_FIELD || name == AFTER_FIELD
}

fn eval_fields(fields: &[crate::ast::Field], scope: &Scope) -> Result<Map, EvalError> {
    let mut inputs = Map::new();
    for field in fields {
        if is_unhashed_field(&field.name) {
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
        if is_unhashed_field(&field.name) {
            continue;
        }
        let value = expr::eval_lenient(&field.value, scope)?;
        inputs.insert(field.name.clone(), value);
    }
    Ok(inputs)
}

fn evaluate_concurrency(node: &DagNode, scope: &Scope) -> Result<Option<ConcurrencyLimit>, EngineError> {
    let Some(field) = node.fields.iter().find(|field| field.name == CONCURRENCY_FIELD) else {
        return Ok(node.resource.default_concurrency().map(|limit| ConcurrencyLimit {
            group: node.concurrency_group.clone(),
            limit,
        }));
    };
    let value = expr::eval(&field.value, scope).map_err(|source| EngineError::Eval {
        pos: node.pos.clone(),
        target: node.name.clone(),
        source,
    })?;
    let limit = value
        .as_number()
        .and_then(|number| number.to_string().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .ok_or_else(|| EngineError::InvalidConcurrency {
            pos: node.pos.clone(),
            target: node.name.clone(),
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
            let node = dag.get_node(name).expect("target in order");
            Ok((name.clone(), evaluate_concurrency(node, scope)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::file_tracker::FileTracker;
    use crate::loader;
    use crate::parser;
    use crate::provider::{DynResource, FuncSignature, Provider, ProviderRegistry, Resource, Target};
    use crate::providers::exec::ExecProvider;
    use crate::state::StateStore;

    fn test_tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::new())
    }

    fn test_cache() -> BuildCache {
        BuildCache::local_only(std::path::Path::new("."))
    }

    fn test_registry(tracker: &Arc<FileTracker>) -> ProviderRegistry {
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
        fn load(&self, target: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(self.data.read().unwrap().get(target).cloned())
        }
        fn save(&self, target: &str, state: &serde_json::Value) -> Result<(), StateError> {
            self.data.write().unwrap().insert(target.into(), state.clone());
            Ok(())
        }
        fn remove(&self, target: &str) -> Result<(), StateError> {
            self.data.write().unwrap().remove(target);
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(self.data.read().unwrap().keys().cloned().collect())
        }
    }

    fn load_and_apply(input: &str) -> Result<Vec<TargetPlan>, EngineError> {
        let tracker = test_tracker();
        let module = parser::parse(input, "<test>").expect("parse failed");
        let store = MemoryStore::new();
        let (mut dag, base) =
            loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).expect("load failed");
        let output = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker)
    }

    #[test]
    fn apply_simple_target() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let results = load_and_apply(&input).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plan.action, PlanAction::Create);
        assert!(output.exists());
    }

    #[test]
    fn apply_reports_changes_since_last_success() {
        use std::path::{Path, PathBuf};
        use std::sync::Mutex;

        #[derive(Debug, serde::Deserialize, bit_derive::Schema)]
        struct Inputs {}

        #[derive(Debug, serde::Serialize, bit_derive::Schema)]
        struct Outputs {
            passed: bool,
        }

        struct RecordProvider {
            seen: Arc<Mutex<Vec<Changes>>>,
        }

        impl Provider for RecordProvider {
            fn name(&self) -> &str {
                "record"
            }
            fn resources(&self) -> Vec<Box<dyn DynResource>> {
                vec![Box::new(RecordResource {
                    seen: self.seen.clone(),
                })]
            }
            fn functions(&self) -> Vec<FuncSignature> {
                vec![]
            }
            fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
                Err(format!("unknown function: {name}").into())
            }
        }

        /// A test resource over `a.txt` and `b.txt` that fails while a
        /// `fail` file, which it does not resolve, exists.
        struct RecordResource {
            seen: Arc<Mutex<Vec<Changes>>>,
        }

        impl Resource for RecordResource {
            type State = ();
            type Inputs = Inputs;
            type Outputs = Outputs;

            fn name(&self) -> &str {
                "check"
            }
            fn kind(&self) -> ResourceKind {
                ResourceKind::Test
            }
            fn resolve(&self, inputs: &Target<Inputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
                let mut map = BTreeMap::new();
                for name in ["a.txt", "b.txt"] {
                    let path = PathBuf::from(inputs.path(name));
                    map.insert(path.to_string_lossy().into_owned(), crate::providers::hash_file(&path)?);
                }
                Ok(map)
            }
            fn plan(&self, _inputs: &Target<Inputs>, prior_state: Option<&()>) -> Result<PlanResult, BoxError> {
                Ok(PlanResult {
                    action: if prior_state.is_some() {
                        PlanAction::None
                    } else {
                        PlanAction::Create
                    },
                    description: "check".into(),
                    reason: None,
                })
            }
            fn apply(
                &self,
                inputs: &Target<Inputs>,
                _prior_state: Option<&()>,
                changes: &Changes,
                _writer: &TargetWriter,
            ) -> Result<ApplyResult<(), Outputs>, BoxError> {
                self.seen.lock().unwrap().push(changes.clone());
                Ok(ApplyResult {
                    outputs: Outputs {
                        passed: !Path::new(&inputs.path("fail")).exists(),
                    },
                    state: Some(()),
                })
            }
            fn destroy(&self, _prior_state: &Target<()>, _writer: &TargetWriter) -> Result<(), BoxError> {
                Ok(())
            }
            fn cache_policy(&self, _inputs: &Target<Inputs>) -> CachePolicy {
                CachePolicy::Local
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        let input = format!("check = record.check {{\n  dir = \"{}\"\n}}\n", root.display());
        let module = parser::parse(&input, "<test>").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(RecordProvider { seen: seen.clone() }));
        let store = MemoryStore::new();
        let cache = BuildCache::local_only(&root);
        let run = |forced: bool| {
            let tracker = test_tracker();
            let (mut dag, base) = loader::load(&module, &Map::new(), &registry, &store, &[]).unwrap();
            let order = resolve_order(&dag, &[]).unwrap();
            let output = Output::new(&[]);
            let result = if forced {
                apply_selected_forced(&mut dag, &base, &store, &cache, &output, &order, 1, &tracker)
            } else {
                apply_selected(&mut dag, &base, &store, &cache, &output, &order, 1, &tracker)
            };
            result.is_ok()
        };
        let last = || seen.lock().unwrap().last().cloned().unwrap();
        let paths =
            |names: &[&str]| Changes::Paths(names.iter().map(|name| root.join(name)).collect::<BTreeSet<PathBuf>>());

        assert!(run(false));
        assert_eq!(last(), Changes::All);

        std::fs::write(root.join("a.txt"), "a2").unwrap();
        assert!(run(false));
        assert_eq!(last(), paths(&["a.txt"]));

        std::fs::write(root.join("b.txt"), "b2").unwrap();
        std::fs::write(root.join("fail"), "").unwrap();
        assert!(!run(false));
        assert_eq!(last(), paths(&["b.txt"]));

        // The rerun after a failure still sees what the failed run tested.
        std::fs::remove_file(root.join("fail")).unwrap();
        assert!(run(false));
        assert_eq!(last(), paths(&["b.txt"]));

        let applies = seen.lock().unwrap().len();
        assert!(run(false));
        assert_eq!(seen.lock().unwrap().len(), applies, "unchanged target must not run");

        assert!(run(true));
        assert_eq!(last(), Changes::All);
    }

    #[test]
    fn matrix_concurrency_does_not_limit_unrelated_targets() {
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

            fn resolve(&self, _inputs: &Target<Inputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
                Ok(BTreeMap::new())
            }

            fn plan(&self, _inputs: &Target<Inputs>, _prior_state: Option<&()>) -> Result<PlanResult, BoxError> {
                Ok(PlanResult {
                    action: PlanAction::Create,
                    description: "probe concurrency".into(),
                    reason: None,
                })
            }

            fn apply(
                &self,
                inputs: &Target<Inputs>,
                _prior_state: Option<&()>,
                _changes: &Changes,
                _writer: &TargetWriter,
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

            fn destroy(&self, _prior_state: &Target<()>, _writer: &TargetWriter) -> Result<(), BoxError> {
                Ok(())
            }

            fn cache_policy(&self, _inputs: &Target<Inputs>) -> CachePolicy {
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

        let matrix_node = dag.get_node(r#"work["a"]"#).unwrap();
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
    fn upstream_outputs_are_checked_before_a_target_runs() {
        // exec's values command declares no types, so the loader cannot
        // check `version.dir`.
        let input = r#"
version = exec { command = "true" values = 'echo "{\"dir\": [\"a\"]}"' }
build = exec { command = "true" dir = version.dir }
"#;
        let error = load_and_apply(input).err().expect("a list dir should fail");
        assert_eq!(
            error.to_string(),
            "<test>:3:1: target 'build': field 'dir': expected string, got list"
        );
    }

    fn go_lint_concurrency_limits(input: &str) -> HashMap<String, Option<ConcurrencyLimit>> {
        let tracker = test_tracker();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(crate::providers::go::GoProvider::new(tracker.clone())));
        let module = parser::parse(input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, base) = loader::load(&module, &Map::new(), &registry, &store, &[]).unwrap();
        let order = dag.topo_order().unwrap();
        concurrency_limits(&dag, &base.scope, &order).unwrap()
    }

    #[test]
    fn go_lint_runs_one_matrix_slice_at_a_time_by_default() {
        let limits = go_lint_concurrency_limits(
            r#"
let module = ["a", "b"]
lint[module] = go.lint { dir = module }
"#,
        );
        let limit = limits[r#"lint["a"]"#].as_ref().expect("go.lint has a default limit");
        assert_eq!((limit.group.as_str(), limit.limit), ("lint", 1));
    }

    #[test]
    fn explicit_concurrency_overrides_go_lint_default() {
        let limits = go_lint_concurrency_limits(
            r#"
let module = ["a", "b"]
lint[module] = go.lint {
  concurrency = 2
  dir = module
}
"#,
        );
        let limit = limits[r#"lint["a"]"#].as_ref().expect("explicit limit");
        assert_eq!(limit.limit, 2);
    }

    #[test]
    fn apply_chain_passes_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
            "build = exec {{\n  command = \"echo hi\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
            "build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
    fn protected_target_skips_destroy() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
    fn explicitly_named_protected_target_is_destroyed() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\ngroup all = [build]\n",
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
        destroy(&mut dag, &store, &out, &["all".into()], false).unwrap();
        assert!(!store.list().unwrap().is_empty());

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["build".into()], false).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn force_does_not_destroy_implicitly_selected_protected_target() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let input = format!(
            "protected build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let out = Output::new(&[]);
        apply(&mut dag, &base, &store, &test_cache(), &out, &[], 1, &tracker).unwrap();

        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &[], true).unwrap();
        assert!(!store.list().unwrap().is_empty());
    }

    #[test]
    fn force_continues_past_destroy_errors() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let out_a = dir.path().join("a.txt");
        let out_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n  clean = \"false\"\n}}\n",
                "b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
        // With force, still returns error but processes all targets
        let result = destroy(&mut dag, &store, &out, &[], true);
        assert!(result.is_err());
        // "b" should have been cleaned despite "a" failing first
        // (reverse order: b first, then a — a fails but b already succeeded)
        // Both states should be removed since we remove state even on error in force mode
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn explicit_target_excluded_from_wildcard_selection() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();

        // `bit` (no targets, no default) skips explicit targets
        assert_eq!(resolve_order(&dag, &[]).unwrap(), vec!["a".to_owned()]);
        // `bit ...` also skips explicit targets
        assert_eq!(resolve_order(&dag, &["...".into()]).unwrap(), vec!["a".to_owned()]);
        // Naming the target directly still runs it
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
            ..Default::default()
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &changed_a, &tracker).unwrap(),
            vec!["a".to_owned(), "c".to_owned()]
        );

        let changed_c = crate::git::Changes {
            paths: HashSet::from([c]),
            ..Default::default()
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &changed_c, &tracker).unwrap(),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn affected_order_compares_inputs_git_cannot_see_with_the_prior_run() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen.txt");
        let unseen = dir.path().join("unseen.txt");
        let out = dir.path().join("dist");
        std::fs::write(&seen, "seen").unwrap();
        std::fs::write(&unseen, "unseen").unwrap();
        let input = format!(
            concat!(
                "seen = exec {{ command = \"mkdir -p '{2}' && touch '{2}/a'\" inputs = [\"{0}\"] outputs = \"{2}/\" }}\n",
                "unseen = exec {{ command = \"true\" inputs = [\"{1}\"] }}\n",
            ),
            seen.display(),
            unseen.display(),
            out.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let cache = BuildCache::local_only(dir.path());
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        apply(&mut dag, &base, &store, &cache, &Output::new(&[]), &[], 1, &tracker).unwrap();
        // Git sees `seen.txt`, but not `unseen.txt` or the ignored output.
        let changes = crate::git::Changes {
            visible: HashSet::from([seen.clone()]),
            ..Default::default()
        };
        let affected = || {
            let (dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            let order = resolve_order(&dag, &[]).unwrap();
            affected_order(&dag, &base, &cache, &order, &changes, &tracker).unwrap()
        };

        assert!(affected().is_empty(), "nothing changed");
        std::fs::write(&seen, "edited").unwrap();
        std::fs::write(&unseen, "edited").unwrap();
        assert_eq!(affected(), ["unseen"], "Git reports changes to the files it sees");
    }

    #[test]
    fn affected_order_always_selects_targets_with_only_a_directory_output() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen");
        let input = format!(
            "build = exec {{ command = \"mkdir -p '{0}' && touch '{0}/a'\" outputs = \"{0}/\" }}\n",
            out.display()
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let cache = BuildCache::local_only(dir.path());
        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        apply(&mut dag, &base, &store, &cache, &Output::new(&[]), &[], 1, &tracker).unwrap();

        let (dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        let order = resolve_order(&dag, &[]).unwrap();
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &crate::git::Changes::default(), &tracker).unwrap(),
            ["build"]
        );
    }

    #[test]
    fn affected_order_selects_targets_whose_tool_version_changed() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        std::fs::write(&source, "source").unwrap();
        let input = format!(
            "build = exec {{ command = \"true\" inputs = [\"{}\"] }}\n",
            source.display()
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let cache = BuildCache::local_only(dir.path());
        let affected = |store: &MemoryStore| {
            let (dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), store, &[]).unwrap();
            let order = resolve_order(&dag, &[]).unwrap();
            affected_order(&dag, &base, &cache, &order, &crate::git::Changes::default(), &tracker).unwrap()
        };

        let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        apply(&mut dag, &base, &store, &cache, &Output::new(&[]), &[], 1, &tracker).unwrap();
        assert!(affected(&store).is_empty(), "nothing changed");

        // Record a different bash version than the one installed, as if it
        // was upgraded since the target ran.
        let mut wrapped: WrappedState = serde_json::from_value(store.load("build").unwrap().unwrap()).unwrap();
        let version = wrapped
            .resolve_map
            .iter_mut()
            .find(|(key, _)| key.starts_with("version:bash"))
            .expect("bash version is tracked");
        *version.1 = SHA256::digest(b"another version");
        store.save("build", &serde_json::to_value(&wrapped).unwrap()).unwrap();
        assert_eq!(affected(&store), vec!["build".to_owned()]);
    }

    #[test]
    fn affected_order_keeps_unmapped_targets_and_falls_back_for_unknown_deletions() {
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
            ..Default::default()
        };
        assert_eq!(
            affected_order(&dag, &base, &cache, &order, &deletion, &tracker).unwrap(),
            order
        );
    }

    #[test]
    fn explicit_target_pulled_in_as_dependency() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        // `b` depends on `a`; `a` is explicit. Selecting `b` must still pull in `a`.
        let input = format!(
            concat!(
                "explicit a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
        // matching the contract that `...` is purely a "non-explicit targets" filter.
        assert_eq!(resolve_order(&dag, &["...".into()]).unwrap(), vec!["b".to_owned()]);
        // `...` is composable: naming the explicit target alongside `...` unions
        // the wildcard expansion with the named target (and its deps).
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
        // `a` is a regular target selected by `...`; `b` is explicit and must
        // be named to participate.
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
            ),
            file_a.display(),
            file_a.display(),
            file_b.display(),
            file_b.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();
        let (dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();

        // `bit ... b` should run every non-explicit target plus the named explicit one.
        let mut order = resolve_order(&dag, &["...".into(), "b".into()]).unwrap();
        order.sort();
        assert_eq!(order, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn explicit_target_skipped_by_destroy_wildcard() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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

        // Destroying with `...` should leave the explicit target's state in place.
        let (mut dag, _base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &out, &["...".into()], false).unwrap();
        assert_eq!(store.list().unwrap(), vec!["b".to_owned()]);
    }

    #[test]
    fn destroy_wildcard_composes_with_named_target() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "explicit b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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

        // `bit -c ... b` should destroy every non-explicit target AND the named explicit one.
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
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "c = exec {{\n  command = \"cp #{{b.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "c = exec {{\n  command = \"cp #{{b.path}} {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
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
    fn group_filters_targets() {
        let tracker = test_tracker();
        let dir = tempfile::tempdir().unwrap();
        let out_a = dir.path().join("a.txt");
        let out_b = dir.path().join("b.txt");
        let input = format!(
            concat!(
                "a = exec {{\n  command = \"echo a > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "b = exec {{\n  command = \"echo b > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
                "group just_a = [a]\n",
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
            "build = exec {{\n  command = \"echo hello > {}\"\n  outputs = \"{}\"\n  inputs = []\n}}\n",
            output.display(),
            output.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let store = MemoryStore::new();

        // First apply creates the target
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
            "build = exec {{\n  command = \"cp {} {}\"\n  outputs = \"{}\"\n  inputs = [\"{}\"]\n}}\n",
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

    #[test]
    fn in_place_exec_uses_changed_input_in_shared_cache_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("BUILD.bit");
        let cache = BuildCache::open_at(dir.path(), &dir.path().join("cache"));
        let tracker = test_tracker();
        let store = MemoryStore::new();
        let output = Output::new(&[]);
        let input = format!(
            "fmt_build = exec {{\n  command = \"tr a-z A-Z < '{}' > '{}.tmp' && mv '{}.tmp' '{}'\"\n  inputs = [\"{}\"]\n  outputs = \"{}\"\n}}\n",
            file.display(),
            file.display(),
            file.display(),
            file.display(),
            file.display(),
            file.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let run = || {
            let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            apply(&mut dag, &base, &store, &cache, &output, &[], 1, &tracker).unwrap()
        };

        std::fs::write(&file, "first").unwrap();
        assert_eq!(run()[0].plan.action, PlanAction::Create);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "FIRST");

        std::fs::write(&file, "second").unwrap();
        assert_eq!(run()[0].plan.action, PlanAction::Update);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "SECOND");
        assert_eq!(run()[0].plan.action, PlanAction::None);
    }

    #[test]
    fn declared_output_drift_still_triggers_local_update() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("out.txt");
        let tracker = test_tracker();
        let store = MemoryStore::new();
        let output = Output::new(&[]);
        let input = format!(
            "build = exec {{\n  command = \"printf ok > '{}'\"\n  outputs = \"{}\"\n}}\n",
            file.display(),
            file.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let run = || {
            let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker).unwrap()
        };

        assert_eq!(run()[0].plan.action, PlanAction::Create);
        assert_eq!(run()[0].plan.action, PlanAction::None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(run()[0].plan.action, PlanAction::Update);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ok");
    }

    #[test]
    fn changing_dir_reruns_the_target_and_clean_uses_the_dir_it_ran_in() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let tracker = test_tracker();
        let store = MemoryStore::new();
        let output = Output::new(&[]);
        let target = |dir: &Path| {
            let input = format!(
                "build = exec {{\n  command = \"touch out\"\n  dir = \"{}\"\n  outputs = \"out\"\n}}\n",
                dir.display()
            );
            parser::parse(&input, "<test>").unwrap()
        };
        let run = |dir: &Path| {
            let (mut dag, base) =
                loader::load(&target(dir), &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker).unwrap()
        };

        assert_eq!(run(&a)[0].plan.action, PlanAction::Create);
        assert_eq!(run(&a)[0].plan.action, PlanAction::None);
        assert_eq!(run(&b)[0].plan.action, PlanAction::Update);
        assert!(b.join("out").is_file(), "the command runs in `dir`");

        // A target that no longer exists is destroyed in the `dir` it ran in.
        let (mut dag, _) = loader::load(&target(&b), &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &output, &[], false).unwrap();
        assert!(!b.join("out").exists());
    }

    #[test]
    fn changing_env_reruns_the_target_and_clean_uses_the_env_it_ran_with() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker();
        let store = MemoryStore::new();
        let output = Output::new(&[]);
        let target = |value: &str| {
            let input = format!(
                "build = exec {{\n  command = \"echo $FOO > out\"\n  clean = \"echo $FOO > cleaned\"\n  \
                 dir = \"{}\"\n  env = {{ FOO = \"{value}\" }}\n  outputs = \"out\"\n}}\n",
                dir.path().display()
            );
            parser::parse(&input, "<test>").unwrap()
        };
        let run = |value: &str| {
            let (mut dag, base) =
                loader::load(&target(value), &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker).unwrap()
        };

        assert_eq!(run("a")[0].plan.action, PlanAction::Create);
        assert_eq!(run("a")[0].plan.action, PlanAction::None);
        assert_eq!(run("b")[0].plan.action, PlanAction::Update);
        assert_eq!(std::fs::read_to_string(dir.path().join("out")).unwrap(), "b\n");

        let (mut dag, _) = loader::load(&target("b"), &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
        destroy(&mut dag, &store, &output, &[], false).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("cleaned")).unwrap(), "b\n");
    }

    #[test]
    fn recorded_env_omits_secrets() {
        let mut env = Map::new();
        env.insert("PLAIN".into(), Value::Str("a".into()));
        env.insert(
            "TOKEN".into(),
            Value::Secret {
                key: "TOKEN".into(),
                value: "s3cr3t".into(),
            },
        );
        let mut inputs = Map::new();
        inputs.insert(ENV_FIELD.into(), Value::Map(Type::String, env));
        assert_eq!(
            recorded_env(&inputs),
            BTreeMap::from([("PLAIN".to_owned(), "a".to_owned())])
        );
        assert_eq!(crate::provider::target_env(&inputs).len(), 2);
    }

    #[test]
    fn destroy_uses_dir_recorded_by_the_resource_before_the_engine_managed_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cleaned");
        let state = serde_json::json!({
            "command": "true",
            "clean": "touch cleaned",
            "dir": dir.path(),
        });
        let prior = unwrap_state(&serde_json::json!({
            "state": state,
            "outputs": {},
            "content_hash": SHA256::ZERO,
        }));
        assert_eq!(prior.dir.as_deref(), dir.path().to_str());
        let registry = test_registry(&test_tracker());
        let resource = registry.get_resource("exec", "exec").unwrap();
        let output = Output::new(&[]);
        let prior_state = Target::new(prior.dir, prior.provider_state.unwrap());
        resource.destroy(&prior_state, &output.writer("build")).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn declared_output_directory_drift_triggers_local_update() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen");
        let file = out.join("nested/out.txt");
        let tracker = test_tracker();
        let store = MemoryStore::new();
        let output = Output::new(&[]);
        let input = format!(
            "build = exec {{\n  command = \"mkdir -p '{}' && printf ok > '{}'\"\n  outputs = \"{}/\"\n}}\n",
            file.parent().unwrap().display(),
            file.display(),
            out.display(),
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let run = || {
            let (mut dag, base) = loader::load(&module, &Map::new(), &test_registry(&tracker), &store, &[]).unwrap();
            apply(&mut dag, &base, &store, &test_cache(), &output, &[], 1, &tracker).unwrap()
        };

        assert_eq!(run()[0].plan.action, PlanAction::Create);
        assert_eq!(run()[0].plan.action, PlanAction::None);
        std::fs::write(&file, "edited").unwrap();
        assert_eq!(run()[0].plan.action, PlanAction::Update);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ok");
        std::fs::remove_dir_all(&out).unwrap();
        assert_eq!(run()[0].plan.action, PlanAction::Update);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ok");
    }

    // -- Shared cache -----------------------------------------------------

    mod cached {
        //! Test provider whose `file` resource writes `<dir>/out.txt` from
        //! `<dir>/src.txt` (or a literal `content`) and shares the output
        //! through the CAS. `check` is the same resource as a test kind whose
        //! `passed` output is controlled by an input.
        use std::collections::BTreeMap;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use serde::{Deserialize, Serialize};

        use crate::cache::{ArtifactRef, Cas};
        use crate::output::TargetWriter;
        use crate::provider::{
            Ambient, ApplyResult, BoxError, CachePolicy, Changes, DynResource, FuncSignature, MaterializeResult,
            OutputFile, PlanAction, PlanResult, Provider, Resource, ResourceKind, Target,
        };
        use crate::sha256::SHA256;
        use crate::value::Value;

        #[derive(Debug, Deserialize, bit_derive::Schema)]
        pub struct Inputs {
            #[serde(default)]
            pub content: Option<String>,
            #[serde(default)]
            pub passed: Option<bool>,
            #[serde(default)]
            pub tool: Option<String>,
            /// A variable the resource reports reading once `apply` has
            /// left a record of it behind, as rustc does in dep-info.
            #[serde(default)]
            pub recorded: Option<String>,
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

        /// Left behind by `apply` when `recorded` is set, like a dep-info file.
        const RECORD: &str = "record.d";

        fn out_path(inputs: &Target<Inputs>) -> PathBuf {
            PathBuf::from(inputs.path("out.txt"))
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
            fn resolve(&self, inputs: &Target<Inputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
                let mut map = BTreeMap::new();
                let path = PathBuf::from(inputs.path("src.txt"));
                if path.is_file() {
                    map.insert(path.to_string_lossy().into_owned(), crate::providers::hash_file(&path)?);
                }
                Ok(map)
            }
            fn ambient(&self, inputs: &Target<Inputs>) -> Vec<Ambient> {
                let mut ambient: Vec<Ambient> = inputs.tool.iter().cloned().map(Ambient::Executable).collect();
                if std::path::Path::new(&inputs.path(RECORD)).is_file() {
                    ambient.extend(inputs.recorded.iter().cloned().map(Ambient::RecordedVariable));
                }
                ambient
            }
            fn plan(&self, _inputs: &Target<Inputs>, prior: Option<&State>) -> Result<PlanResult, BoxError> {
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
                inputs: &Target<Inputs>,
                _prior: Option<&State>,
                _changes: &Changes,
                _writer: &TargetWriter,
            ) -> Result<ApplyResult<State, Outputs>, BoxError> {
                self.applies.fetch_add(1, Ordering::SeqCst);
                let out = out_path(inputs);
                let content = match &inputs.content {
                    Some(c) => c.clone(),
                    None => std::fs::read_to_string(inputs.path("src.txt"))?,
                };
                std::fs::write(&out, content)?;
                std::fs::set_permissions(&out, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
                if inputs.recorded.is_some() {
                    std::fs::write(inputs.path(RECORD), "")?;
                }
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
            fn destroy(&self, state: &Target<State>, _writer: &TargetWriter) -> Result<(), BoxError> {
                let _ = std::fs::remove_file(&state.path);
                Ok(())
            }
            fn cache_policy(&self, _inputs: &Target<Inputs>) -> CachePolicy {
                CachePolicy::Shared { version: 1 }
            }
            fn output_keys(&self, inputs: &Target<Inputs>) -> Vec<String> {
                vec![out_path(inputs).to_string_lossy().into_owned()]
            }
            /// Capture and check come from the default implementations. Only
            /// the state and outputs are rebuilt here, because they hold this
            /// worktree's absolute output path.
            fn materialize(
                &self,
                inputs: &Target<Inputs>,
                _state: &State,
                outputs: &[OutputFile],
                artifacts: &BTreeMap<String, ArtifactRef>,
                cas: &Cas,
                writer: &TargetWriter,
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

        fn target(&self, name: &str, resource: &str, extra: &str) -> String {
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
            reg.register(Box::new(ExecProvider::new(test_tracker())));
            reg
        }

        fn cache(&self, wt: &Worktree) -> BuildCache {
            BuildCache::open_at(&wt.root, &self.cache_dir)
        }

        fn load(&self, wt: &Worktree, input: &str) -> (Dag, BaseScope, Arc<FileTracker>) {
            let tracker = test_tracker();
            let module = parser::parse(input, "<test>").expect("parse failed");
            let (dag, base) =
                loader::load(&module, &Map::new(), &self.registry(), &wt.store, &[]).expect("load failed");
            (dag, base, tracker)
        }

        fn apply(&self, wt: &Worktree, input: &str, jobs: usize) -> Result<Vec<TargetPlan>, EngineError> {
            self.apply_with(wt, input, jobs, &self.cache(wt))
        }

        fn apply_with(
            &self,
            wt: &Worktree,
            input: &str,
            jobs: usize,
            cache: &BuildCache,
        ) -> Result<Vec<TargetPlan>, EngineError> {
            let (mut dag, base, tracker) = self.load(wt, input);
            let output = Output::new(&[]);
            apply(&mut dag, &base, &wt.store, cache, &output, &[], jobs, &tracker)
        }

        fn plan(&self, wt: &Worktree, input: &str) -> Vec<TargetPlan> {
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

        /// Every variant under the single published action key, parsed.
        fn only_variants(&self) -> Vec<Receipt> {
            let paths = self.receipts();
            assert_eq!(paths.len(), 1, "expected exactly one action key");
            serde_json::from_slice(&std::fs::read(&paths[0]).unwrap()).unwrap()
        }

        /// The single published receipt, parsed.
        fn only_receipt(&self) -> Receipt {
            let mut variants = self.only_variants();
            assert_eq!(variants.len(), 1, "expected exactly one receipt");
            variants.remove(0)
        }
    }

    fn wrapped(store: &MemoryStore, target: &str) -> WrappedState {
        serde_json::from_value(store.load(target).unwrap().unwrap()).unwrap()
    }

    /// A variable known only from what a build left behind is recorded in
    /// the receipt rather than the key, so a fresh worktree, which has no
    /// such record yet, computes the same key and reuses the result.
    #[test]
    fn recorded_variables_are_checked_rather_than_keyed() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.target("app", "file", "  recorded = \"PATH\"\n"), 1)
            .unwrap();
        let receipt = h.only_receipt();
        assert_eq!(
            receipt.variables,
            [(
                "PATH".to_owned(),
                crate::providers::variable_fingerprint("PATH", h.cache(&a).paths())
            )]
            .into()
        );

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h
            .apply(&b, &b.target("app", "file", "  recorded = \"PATH\"\n"), 1)
            .unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(h.applies(), 1);
    }

    #[test]
    fn a_receipt_whose_recorded_variable_changed_is_not_reused() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let target = |wt: &Worktree| wt.target("app", "file", "  recorded = \"PATH\"\n");
        h.apply(&a, &target(&a), 1).unwrap();
        // Stand in for a build that ran with a different PATH.
        let path = h.receipts().remove(0);
        let mut variants = h.only_variants();
        variants[0]
            .variables
            .insert("PATH".into(), SHA256::digest(b"elsewhere"));
        std::fs::write(&path, serde_json::to_vec(&variants).unwrap()).unwrap();

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &target(&b), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 2);
        assert_eq!(
            h.only_variants().len(),
            2,
            "the new value is published as another variant"
        );
    }

    /// Keys hold repository-relative paths, so a separate clone, not just a
    /// linked worktree, reuses a result.
    #[test]
    fn a_separate_clone_reuses_a_result() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();

        let other = h.base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q"]);
        let b = Worktree {
            root: other.canonicalize().unwrap(),
            store: MemoryStore::new(),
        };
        b.write_src("hello");
        let plans = h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(h.applies(), 1);
    }

    #[test]
    fn state_with_keys_in_an_older_format_reports_every_input_changed() {
        let state = serde_json::json!({
            "state": {},
            "outputs": {},
            "content_hash": SHA256::digest(b"c"),
            "input_hash": SHA256::digest(b"i"),
            "resolve_map": { "src/main.rs": SHA256::digest(b"m") },
        });
        let prior = unwrap_state(&state);
        assert_eq!(prior.input_hash, SHA256::ZERO);
        assert!(prior.resolve_map.is_empty());
        assert_eq!(
            prior.content_hash,
            SHA256::digest(b"c"),
            "an unchanged target still skips"
        );
    }

    /// A result built on one machine reaches another through a remote
    /// cache, whose disk cache starts empty.
    #[test]
    fn restores_a_result_another_machine_uploaded() {
        use crate::cache::{Disk, Remote};
        let h = Harness::new();
        let machine = |name: &str, wt: &Worktree, writable: bool| {
            let remote = Disk::new(&h.base.join("remote"));
            BuildCache::open_at(&wt.root, &h.base.join(name)).with_remote(Remote::new(Box::new(remote), writable))
        };

        let a = h.worktree("a");
        a.write_src("hello");
        h.apply_with(&a, &a.target("app", "file", ""), 1, &machine("ci", &a, true))
            .unwrap();
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        let dev = machine("dev", &b, false);
        let plans = h.apply_with(&b, &b.target("app", "file", ""), 1, &dev).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(h.applies(), 1, "provider must not run on a remote hit");
        assert_eq!(std::fs::read_to_string(b.out()).unwrap(), "hello");
    }

    #[test]
    fn restores_artifact_after_source_worktree_is_deleted() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let plans = h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 1);
        assert_eq!(h.receipts().len(), 1);
        assert_eq!(h.blobs().len(), 1);
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
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

        // The restored target is now settled locally.
        let plans = h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(h.applies(), 1);
    }

    /// Load `publish name=app`, whose target reads the secret `key`, with the
    /// secret set to `value`. Returns the instance name, its input hash and
    /// its shared-cache key.
    fn load_secret_instance(key: &str, value: &str) -> (String, SHA256, ActionKey) {
        // SAFETY: only this test reads or writes this variable.
        unsafe { std::env::set_var(key, value) };
        let input = format!(
            "let token = secret(\"{key}\")\npublish(name : string) = exec {{\n  command = token\n  outputs = \"out\"\n}}\n"
        );
        let module = parser::parse(&input, "<test>").unwrap();
        let invocations = [crate::invocation::Invocation {
            name: "publish".into(),
            args: vec![("name".into(), "app".into())],
        }];
        let tracker = test_tracker();
        let (dag, base, selected) = loader::load_selected(
            &module,
            &Map::new(),
            &test_registry(&tracker),
            &MemoryStore::new(),
            &[],
            &invocations,
        )
        .unwrap();
        let name = selected[0].clone();
        let inputs = eval_fields(&dag.get_node(&name).unwrap().fields, &base.scope).unwrap();
        let input_hash = hash_inputs(&inputs, &test_cache());
        let key = ActionKeyInput {
            receipt_version: RECEIPT_VERSION,
            provider: "exec",
            resource: "exec",
            cache_version: 1,
            target: &name,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            inputs: input_hash,
            sources: &BTreeMap::new(),
            deps: &BTreeMap::new(),
        }
        .key();
        (name, input_hash, key)
    }

    #[test]
    fn rotated_secret_keeps_instance_name_and_changes_hashes() {
        let key = "BIT_TEST_ROTATED_SECRET";
        let (first_name, first_hash, first_key) = load_secret_instance(key, "first-value");
        let (second_name, second_hash, second_key) = load_secret_instance(key, "second-value");

        assert_eq!(first_name, r#"publish(name="app")"#);
        assert_eq!(first_name, second_name);
        assert_ne!(first_hash, second_hash);
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn dump_shows_secrets_by_key() {
        let secret = Value::Secret {
            key: "TOKEN".into(),
            value: "s3cr3t".into(),
        };
        assert_eq!(dump_scalar(&secret), r#"secret("TOKEN")"#);
    }

    #[test]
    fn changed_tool_reruns_target_and_changes_action_key() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let tool = a.root.join("tool");
        std::fs::write(&tool, "v1").unwrap();
        let target = a.target("app", "file", &format!("  tool = \"{}\"\n", tool.display()));

        assert_eq!(h.apply(&a, &target, 1).unwrap()[0].plan.action, PlanAction::Create);
        assert_eq!(h.apply(&a, &target, 1).unwrap()[0].plan.action, PlanAction::None);

        std::fs::write(&tool, "v2").unwrap();
        let plans = h.apply(&a, &target, 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Update);
        assert_eq!(plans[0].plan.reason.as_deref(), Some("'tool' changed"));
        assert_eq!(h.applies(), 2);
        assert_eq!(h.receipts().len(), 2, "the tool is part of the action key");

        // The output is identical under both tools, so the v1 receipt is
        // bound as already valid.
        std::fs::write(&tool, "v1").unwrap();
        let plans = h.apply(&a, &target, 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(plans[0].plan.reason.as_deref(), Some("cached"));
        assert_eq!(h.applies(), 2);
    }

    #[test]
    fn modifying_restored_output_cannot_corrupt_the_cache() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
        std::fs::write(b.out(), "tampered").unwrap();

        let c = h.worktree("c");
        c.write_src("hello");
        let plans = h.apply(&c, &c.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(std::fs::read_to_string(c.out()).unwrap(), "hello");
        assert_eq!(h.applies(), 1);
    }

    #[test]
    fn different_source_hashes_coexist_and_older_hash_hits() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        a.write_src("v2");
        let plans = h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Update);
        assert_eq!(h.applies(), 2);
        assert_eq!(h.receipts().len(), 2);

        a.write_src("v1");
        let plans = h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert_eq!(h.applies(), 2);
        assert_eq!(std::fs::read_to_string(a.out()).unwrap(), "v1");
    }

    #[test]
    fn declared_outputs_reach_the_cache() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();

        assert_eq!(h.only_receipt().artifacts.keys().collect::<Vec<_>>(), ["out.txt"]);
    }

    /// An excluded output is still kept out of the action key -- it is a
    /// produced file, not a source -- but nothing about it is stored.
    #[test]
    fn uncached_entries_resolve_like_the_outputs_they_name() {
        let h = Harness::new();
        let a = h.worktree("a");
        let target = format!(
            concat!(
                "app = exec {{\n",
                "  command  = \"mkdir -p dist && touch dist/a keep.txt\"\n",
                "  dir      = \"{}/web\"\n",
                "  outputs  = [\"dist/\", \"keep.txt\"]\n",
                "  uncached = [\"dist/\"]\n",
                "}}\n",
            ),
            a.root.display()
        );
        std::fs::create_dir_all(a.root.join("web")).unwrap();
        h.apply(&a, &target, 1).unwrap();

        assert!(a.root.join("web/dist/a").is_file(), "outputs are relative to `dir`");
        assert_eq!(h.only_receipt().artifacts.keys().collect::<Vec<_>>(), ["web/keep.txt"]);
    }

    #[test]
    fn uncached_outputs_are_not_stored() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("v1");
        // Named with the trailing slash `output` uses for directories, which
        // is optional here.
        let target = a.target("app", "file", "  uncached = [\"out.txt/\"]\n");
        h.apply(&a, &target, 1).unwrap();

        assert!(h.only_receipt().artifacts.is_empty());

        // A second worktree still matches the action, because excluding an
        // output changes what is stored, not what the target depends on. It
        // therefore reuses the result without receiving the excluded file --
        // the cost of opting an output out.
        let b = h.worktree("b");
        b.write_src("v1");
        let plans = h
            .apply(&b, &b.target("app", "file", "  uncached = [\"out.txt/\"]\n"), 1)
            .unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::None);
        assert_eq!(h.applies(), 1);
        assert!(!b.out().exists());
    }

    #[test]
    fn valid_receipt_is_bound_without_materializing() {
        let h = Harness::new();
        let a = h.worktree("a");
        h.apply(&a, &a.target("app", "file", "  content = \"hi\"\n"), 1)
            .unwrap();

        // Reproduce what apply would have written, permissions included: a
        // receipt only counts as valid when the destination is byte- and
        // mode-identical to the captured artifact.
        let b = h.worktree("b");
        std::fs::write(b.out(), "hi").unwrap();
        std::fs::set_permissions(b.out(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let plans = h
            .apply(&b, &b.target("app", "file", "  content = \"hi\"\n"), 1)
            .unwrap();
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
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        a.delete();

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.plan(&b, &b.target("app", "file", ""));
        assert_eq!(plans[0].plan.action, PlanAction::Restore);
        assert!(!b.out().exists());
        assert!(b.store.load("app").unwrap().is_none());
    }

    #[test]
    fn failed_tests_never_publish_receipts() {
        let h = Harness::new();
        let a = h.worktree("a");
        let result = h.apply(&a, &a.target("t", "check", "  content = \"x\"\n  passed = false\n"), 1);
        assert!(matches!(result, Err(EngineError::TestFailed { .. })));
        assert!(h.receipts().is_empty());
        assert!(a.store.load("t").unwrap().is_some(), "failure stays in local state");

        h.apply(&a, &a.target("t", "check", "  content = \"x\"\n  passed = true\n"), 1)
            .unwrap();
        assert_eq!(h.receipts().len(), 1);
    }

    #[test]
    fn corrupt_blob_is_a_miss() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        a.delete();
        for blob in h.blobs() {
            std::fs::set_permissions(&blob, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
            std::fs::write(&blob, "garbage").unwrap();
        }

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
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
        h.apply(&a, &a.target("app", "file", ""), 1).unwrap();
        a.delete();
        for receipt in h.receipts() {
            std::fs::write(&receipt, "{ nope").unwrap();
        }

        let b = h.worktree("b");
        b.write_src("hello");
        let plans = h.apply(&b, &b.target("app", "file", ""), 1).unwrap();
        assert_eq!(plans[0].plan.action, PlanAction::Create);
        assert_eq!(h.applies(), 2);
    }

    #[test]
    fn clean_does_not_delete_shared_receipts_or_blobs() {
        let h = Harness::new();
        let a = h.worktree("a");
        a.write_src("hello");
        let input = a.target("app", "file", "");
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
            a.target("a", "file", ""),
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
        let (mut dag, base, tracker) = h.load(&a, &a.target("app", "file", ""));
        let local = BuildCache::local_only(&a.root);
        apply(&mut dag, &base, &a.store, &local, &Output::new(&[]), &[], 1, &tracker).unwrap();
        assert!(h.receipts().is_empty());
        assert!(h.blobs().is_empty());
    }
}
