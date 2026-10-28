use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, DynResource, FuncSignature, PlanResult, Provider,
    Resource, ResourceKind,
};
use crate::sha256::SHA256;
use crate::value::Value;

use super::imports::ImportGraph;

/// Output extensions protoc writes as one archive rather than a directory.
const ARCHIVE_EXTENSIONS: [&str; 3] = ["zip", "jar", "srcjar"];

/// The values protoc takes for an option among `flags`: `--long=value`,
/// `--long value`, `-Svalue` or `-S value` for the `short` letter, if any.
fn flag_values<'a>(flags: &'a [String], long: &str, short: Option<char>) -> Vec<&'a str> {
    let long_flag = format!("--{long}");
    let short_flag = short.map(|short| format!("-{short}"));
    let mut values = Vec::new();
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if *flag == long_flag || short_flag.as_ref() == Some(flag) {
            values.extend(flags.next().map(String::as_str));
        } else if let Some(value) = flag.strip_prefix(&long_flag).and_then(|rest| rest.strip_prefix('=')) {
            values.push(value);
        } else if let Some(value) = short_flag
            .as_deref()
            .filter(|_| !flag.starts_with("--"))
            .and_then(|short| flag.strip_prefix(short))
        {
            values.push(value);
        }
    }
    values
}

/// The paths in a protoc path list, which separates them with `:`.
fn path_list(value: &str) -> impl Iterator<Item = &str> {
    value.split(':').filter(|path| !path.is_empty())
}

fn is_archive(out: &str) -> bool {
    Path::new(out)
        .extension()
        .is_some_and(|extension| ARCHIVE_EXTENSIONS.iter().any(|archive| extension == *archive))
}

/// Generate code from Protobuf files with protoc
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct ProtocGenerateInputs {
    /// Protobuf files or glob patterns to compile, relative to `dir`
    pub sources: Vec<String>,
    /// Import paths passed as `-I` (defaults to `dir`)
    #[serde(default)]
    pub include: Vec<String>,
    /// Output directory for each generator (e.g. `{ go = "gen/go" }` passes `--go_out=gen/go`)
    pub out: HashMap<String, String>,
    /// Options for each generator (e.g. `{ go = "paths=source_relative" }` passes `--go_opt=paths=source_relative`). Options that name an existing file are tracked as inputs.
    #[serde(default)]
    pub opt: HashMap<String, String>,
    /// Extra flags passed to protoc. `-I`/`--proto_path` paths are searched for imports after `include`, `--descriptor_set_in` files are tracked as inputs, and `--descriptor_set_out`/`-o` files as outputs.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Compile only the sources that changed or import a changed file. Every generator must write each output from a single source; descriptor sets, documentation, and merged API specs need full runs.
    #[serde(default)]
    pub incremental: bool,
}

impl ProtocGenerateInputs {
    /// Arguments before the source files, with each output path passed
    /// through `out`. Generators are sorted so the command is the same on
    /// every run.
    fn args(&self, out: impl Fn(&str) -> String) -> Vec<String> {
        let mut args: Vec<String> = self.include.iter().map(|path| format!("-I{path}")).collect();
        let mut outs: Vec<_> = self.out.iter().collect();
        outs.sort();
        for (generator, path) in outs {
            args.push(format!("--{generator}_out={}", out(path)));
        }
        let mut opt: Vec<_> = self.opt.iter().collect();
        opt.sort();
        for (generator, options) in opt {
            args.push(format!("--{generator}_opt={options}"));
        }
        args.extend(self.flags.iter().cloned());
        args
    }
}

impl Block<ProtocGenerateInputs> {
    /// Source files matching `sources`, relative to `dir` as protoc expects.
    fn source_files(&self) -> Result<Vec<String>, BoxError> {
        let dir = self.dir.as_deref().unwrap_or("");
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for source in &self.sources {
            let pattern = if dir.is_empty() || Path::new(source).is_absolute() {
                source.clone()
            } else {
                format!("{}/{source}", glob::Pattern::escape(dir.trim_end_matches('/')))
            };
            let mut matched = Vec::new();
            for path in glob::glob(&pattern)? {
                let path = path?;
                if path.is_file() {
                    let relative = path.strip_prefix(dir).unwrap_or(&path);
                    matched.push(relative.to_string_lossy().into_owned());
                }
            }
            if matched.is_empty() {
                return Err(format!("source `{source}` matched no files").into());
            }
            matched.sort();
            files.extend(matched.into_iter().filter(|file| seen.insert(file.clone())));
        }
        Ok(files)
    }

    /// The `-I` paths protoc resolves imports against, in order: `include`,
    /// then those `flags` add.
    fn roots(&self) -> Vec<PathBuf> {
        let dir = self.dir.as_deref();
        let flagged = flag_values(&self.flags, "proto_path", Some('I'))
            .into_iter()
            .flat_map(path_list);
        let roots: Vec<PathBuf> = self
            .include
            .iter()
            .map(String::as_str)
            .chain(flagged)
            .map(|path| super::normalize(&super::in_dir(dir, path)))
            .collect();
        if roots.is_empty() {
            // protoc imports from its working directory when given no `-I`.
            return vec![super::normalize(&super::in_dir(dir, ""))];
        }
        roots
    }

    /// Files besides `.proto` sources that protoc or its generators read:
    /// the descriptor sets `--descriptor_set_in` names, and generator
    /// options in `opt` that name an existing file, such as a config.
    fn option_files(&self) -> Vec<PathBuf> {
        let dir = self.dir.as_deref();
        let mut files: Vec<PathBuf> = flag_values(&self.flags, "descriptor_set_in", None)
            .into_iter()
            .flat_map(path_list)
            .map(|path| super::in_dir(dir, path))
            .collect();
        // Generator options are `name=value` pairs separated by commas, and
        // which name files is up to each generator.
        for options in self.opt.values() {
            for option in options.split(',') {
                for candidate in [Some(option), option.split_once('=').map(|(_, value)| value)]
                    .into_iter()
                    .flatten()
                {
                    let path = super::in_dir(dir, candidate);
                    if !candidate.is_empty() && path.is_file() {
                        files.push(path);
                    }
                }
            }
        }
        files
    }

    /// The descriptor sets `--descriptor_set_out` or `-o` writes, as bit's
    /// process sees them. protoc writes them itself, outside the staging
    /// directory generators write to.
    fn descriptor_outputs(&self) -> Vec<PathBuf> {
        flag_values(&self.flags, "descriptor_set_out", Some('o'))
            .into_iter()
            .map(|path| super::in_dir(self.dir.as_deref(), path))
            .collect()
    }

    fn compilation(&self) -> Result<Compilation, BoxError> {
        let files = self.source_files()?;
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|file| super::normalize(&super::in_dir(self.dir.as_deref(), file)))
            .collect();
        let graph = ImportGraph::build(&paths, &self.roots())?;
        Ok(Compilation { files, paths, graph })
    }

    /// protoc's version and the plugins its generators run.
    fn ambient(&self) -> Vec<Ambient> {
        // `--plugin=protoc-gen-NAME=PATH` names a plugin explicitly, and
        // `--plugin=PATH` names it by its file name.
        let mut explicit = HashMap::new();
        for flag in &self.flags {
            let Some(plugin) = flag.strip_prefix("--plugin=") else {
                continue;
            };
            let (name, path) = match plugin.split_once('=') {
                Some((name, path)) => (name.to_owned(), path),
                None => (
                    Path::new(plugin)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    plugin,
                ),
            };
            explicit.insert(name, path.to_owned());
        }

        let mut generators: Vec<&String> = self.out.keys().collect();
        generators.sort();
        let dir = self.dir.as_deref();
        let mut tools = vec![super::version("protoc", dir)];
        for generator in generators {
            let plugin = format!("protoc-gen-{generator}");
            if let Some(path) = explicit.get(&plugin) {
                tools.push(super::plugin(path, dir));
            } else if !super::PROTOC_BUILTIN_GENERATORS.contains(&generator.as_str()) {
                tools.push(super::plugin(&plugin, dir));
            }
        }
        tools
    }

    /// Hashes of the compiled files with their imports, and of the tools
    /// that compile them. The tool hashes are the ones the engine records,
    /// so an incremental run sees the same toolchain change.
    fn hashes(&self, compilation: &Compilation, tracker: &FileTracker) -> Result<Hashes, BoxError> {
        Ok(Hashes {
            protos: tracker.hash_files(&compilation.graph.files())?,
            tools: crate::providers::hash_ambient(&self.ambient(), tracker)?,
        })
    }

    /// Generator outputs, as bit's process sees them.
    fn outputs(&self) -> Vec<PathBuf> {
        let mut outputs: Vec<_> = self
            .out
            .values()
            .map(|out| super::in_dir(self.dir.as_deref(), out))
            .collect();
        outputs.sort();
        outputs.dedup();
        outputs
    }
}

/// The block's sources and everything they import.
struct Compilation {
    /// Source files relative to `dir`, as protoc is given them.
    files: Vec<String>,
    /// The same files as bit's process sees them, normalized like the
    /// graph's paths.
    paths: Vec<PathBuf>,
    graph: ImportGraph,
}

struct Hashes {
    protos: BTreeMap<String, SHA256>,
    tools: BTreeMap<String, SHA256>,
}

/// Sources an incremental run compiles, relative to `dir`.
#[derive(Debug, Default, PartialEq)]
struct Batches {
    /// Sources compiled before that changed or import a changed file.
    changed: Vec<String>,
    /// Sources new since the last run.
    added: Vec<String>,
}

/// What an incremental run must compile, or `None` when only a full run
/// is correct.
fn incremental_batches(
    inputs: &Block<ProtocGenerateInputs>,
    prior: &ProtocGenerateState,
    compilation: &Compilation,
    hashes: &Hashes,
) -> Option<Batches> {
    // An archive or descriptor set holds every source's output, so a run
    // over some sources would replace it with a partial one.
    if !inputs.incremental
        || prior.inputs != inputs.value
        || prior.tools != hashes.tools
        || inputs.out.values().any(|out| is_archive(out))
        || !inputs.descriptor_outputs().is_empty()
    {
        return None;
    }
    // A removed source leaves outputs behind that only a full run can
    // identify as stale.
    let compiled: BTreeSet<&String> = prior.compiled.iter().collect();
    let current: BTreeSet<&String> = compilation.files.iter().collect();
    if !compiled.is_subset(&current) {
        return None;
    }

    let changed: BTreeSet<PathBuf> = hashes
        .protos
        .iter()
        .filter(|(path, hash)| prior.protos.get(*path) != Some(hash))
        .map(|(path, _)| PathBuf::from(path))
        .collect();
    let affected: BTreeSet<PathBuf> = compilation
        .graph
        .affected(&compilation.paths, &changed)
        .into_iter()
        .collect();
    // With no changed sources the block re-runs because its outputs changed
    // or were deleted, which only a full run repairs.
    if affected.is_empty() {
        return None;
    }

    let mut batches = Batches::default();
    for (file, path) in compilation.files.iter().zip(&compilation.paths) {
        if !affected.contains(path) {
            continue;
        }
        if compiled.contains(file) {
            batches.changed.push(file.clone());
        } else {
            batches.added.push(file.clone());
        }
    }
    Some(batches)
}

/// Generator output written to a staging directory, so bit knows exactly
/// which files a run produced before any of them replace the real outputs.
struct Staged {
    _dir: tempfile::TempDir,
    /// Each staged file with its destination.
    files: Vec<(PathBuf, PathBuf)>,
}

impl Staged {
    fn destinations(&self) -> impl Iterator<Item = String> + '_ {
        self.files
            .iter()
            .map(|(_, destination)| destination.to_string_lossy().into_owned())
    }

    /// Move the staged files into place, leaving files that already hold the
    /// same content untouched. Returns the destinations.
    fn install(self) -> Result<Vec<String>, BoxError> {
        for (staged, destination) in &self.files {
            if std::fs::read(destination).is_ok_and(|current| std::fs::read(staged).is_ok_and(|new| new == current)) {
                continue;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Renaming fails across filesystems; copying always works.
            if std::fs::rename(staged, destination).is_err() {
                std::fs::copy(staged, destination)
                    .map_err(|e| format!("failed to write {}: {e}", destination.display()))?;
            }
        }
        Ok(self.destinations().collect())
    }
}

/// Run protoc on `files` with every generator writing into a staging
/// directory.
fn stage(inputs: &Block<ProtocGenerateInputs>, files: &[String], writer: &BlockWriter) -> Result<Staged, BoxError> {
    let dir = inputs.dir.as_deref();
    // Staging inside `dir` keeps the final moves on one filesystem, and the
    // hidden name keeps it out of `.proto` globs.
    let staging = tempfile::Builder::new()
        .prefix(".bit-protoc-")
        .tempdir_in(dir.unwrap_or("."))?;
    // protoc runs in `dir`, so staged paths must not be relative to bit's.
    let root = staging.path().canonicalize()?;

    let mut outs: Vec<&String> = inputs.out.values().collect();
    outs.sort();
    outs.dedup();
    // Generators that share an output share a staging directory, which
    // insertion points need: one generator edits another's file in memory.
    let mut staged_outs = HashMap::new();
    for (index, out) in outs.iter().enumerate() {
        let stage_dir = root.join(index.to_string());
        std::fs::create_dir_all(&stage_dir)?;
        let path = match Path::new(out.as_str()).file_name() {
            Some(name) if is_archive(out) => stage_dir.join(name),
            _ => stage_dir,
        };
        staged_outs.insert(out.as_str(), path);
    }

    // An argument file keeps thousands of sources under the OS limit on
    // command-line length.
    let mut args = inputs.args(|out| staged_outs[out].to_string_lossy().into_owned());
    args.extend(files.iter().cloned());
    let arg_file = root.join("args");
    std::fs::write(&arg_file, args.join("\n"))?;
    let status = super::run("protoc", &[format!("@{}", arg_file.display())], dir, writer)?;
    if !status.success() {
        return Err(format!("`protoc` exited with {status}").into());
    }

    let mut staged = Vec::new();
    for out in outs {
        let destination = super::in_dir(dir, out);
        let path = &staged_outs[out.as_str()];
        if is_archive(out) {
            if path.is_file() {
                staged.push((path.clone(), destination));
            }
            continue;
        }
        for file in files_under(path)? {
            let relative = file.strip_prefix(path)?.to_path_buf();
            staged.push((file, destination.join(relative)));
        }
    }
    Ok(Staged {
        _dir: staging,
        files: staged,
    })
}

fn files_under(dir: &Path) -> Result<Vec<PathBuf>, BoxError> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Remove generated `files`, then the directories this empties below the
/// output `roots`. Only files a generator wrote are removed, so anything
/// else kept in an output directory survives.
fn remove_generated(files: &[String], roots: &[PathBuf], writer: &BlockWriter) -> Result<(), BoxError> {
    for file in files {
        let path = Path::new(file);
        crate::providers::remove_path(path, writer)?;
        let mut parent = path.parent();
        while let Some(dir) = parent {
            let below_root = roots.iter().any(|root| dir != root && dir.starts_with(root));
            // Removing a directory that still has entries fails.
            if !below_root || std::fs::remove_dir(dir).is_err() {
                break;
            }
            parent = dir.parent();
        }
    }
    Ok(())
}

/// Outputs from a `protoc.generate` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct ProtocGenerateOutputs {
    /// Generator output directories
    pub paths: Vec<String>,
}

/// Persisted state for a `protoc.generate` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocGenerateState {
    #[serde(flatten)]
    pub inputs: ProtocGenerateInputs,
    /// Source files the outputs were generated from, relative to `dir`.
    #[serde(default)]
    pub compiled: Vec<String>,
    /// Hashes of the sources and the files they import.
    #[serde(default)]
    pub protos: BTreeMap<String, SHA256>,
    /// Hashes of protoc and the plugins that generated the outputs.
    #[serde(default)]
    pub tools: BTreeMap<String, SHA256>,
    /// Files the generators wrote, removed on destroy.
    #[serde(default)]
    pub generated: Vec<String>,
}

pub struct ProtocGenerateResource {
    tracker: Arc<FileTracker>,
}

impl ProtocGenerateResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

/// The protoc command that compiles `sources`, for display. The real run
/// passes the arguments through a file and writes to a staging directory.
fn command_line(inputs: &ProtocGenerateInputs, sources: &[String]) -> String {
    let mut args = inputs.args(str::to_owned);
    args.extend(sources.iter().cloned());
    crate::providers::display_command("protoc", &args)
}

/// Compile only `batches`. Returns every generated file, or `None` when the
/// run showed that a full run is needed.
fn run_incremental(
    inputs: &Block<ProtocGenerateInputs>,
    prior: &ProtocGenerateState,
    batches: &Batches,
    total: usize,
    writer: &BlockWriter,
) -> Result<Option<Vec<String>>, BoxError> {
    // One start line covers both batches, though each runs its own protoc.
    let sources: Vec<String> = batches.changed.iter().chain(&batches.added).cloned().collect();
    writer.starting(&command_line(inputs, &sources));
    writer.line(&format!("compiling {} of {total} sources", sources.len()));
    let mut generated: BTreeSet<String> = prior.generated.iter().cloned().collect();
    if !batches.changed.is_empty() {
        let staged = stage(inputs, &batches.changed, writer)?;
        // An existing source that writes a file it did not write before may
        // have moved its outputs, for example by changing `go_package`. Only
        // a full run shows what it left behind, so nothing is installed.
        if staged.destinations().any(|file| !generated.contains(&file)) {
            writer.line("outputs moved; compiling all sources");
            return Ok(None);
        }
        generated.extend(staged.install()?);
    }
    // New sources write new files, which cannot be stale, so they compile
    // separately from the check above.
    if !batches.added.is_empty() {
        generated.extend(stage(inputs, &batches.added, writer)?.install()?);
    }
    Ok(Some(generated.into_iter().collect()))
}

/// Compile every source and remove generated files the run no longer
/// writes.
fn run_full(
    inputs: &Block<ProtocGenerateInputs>,
    prior: Option<&ProtocGenerateState>,
    files: &[String],
    writer: &BlockWriter,
) -> Result<Vec<String>, BoxError> {
    let generated = stage(inputs, files, writer)?.install()?;
    if let Some(prior) = prior {
        let current: BTreeSet<&String> = generated.iter().collect();
        let stale: Vec<String> = prior
            .generated
            .iter()
            .filter(|file| !current.contains(file))
            .cloned()
            .collect();
        // The prior run's `dir` is not known here, so outputs it left after
        // `dir` changed are removed but their emptied directories remain.
        let prior_inputs = Block::new(inputs.dir.clone(), prior.inputs.clone());
        remove_generated(&stale, &prior_inputs.outputs(), writer)?;
    }
    Ok(generated)
}

impl Resource for ProtocGenerateResource {
    type State = ProtocGenerateState;
    type Inputs = ProtocGenerateInputs;
    type Outputs = ProtocGenerateOutputs;

    fn name(&self) -> &str {
        "generate"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    /// Tracks the sources, the files they transitively import, and the
    /// files flags and generator options name.
    fn resolve(&self, inputs: &Block<ProtocGenerateInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut files = inputs.compilation()?.graph.files();
        files.extend(inputs.option_files());
        self.tracker.hash_files(&files)
    }

    fn ambient(&self, inputs: &Block<ProtocGenerateInputs>) -> Vec<Ambient> {
        inputs.ambient()
    }

    fn plan(
        &self,
        inputs: &Block<ProtocGenerateInputs>,
        prior_state: Option<&ProtocGenerateState>,
    ) -> Result<PlanResult, BoxError> {
        Ok(super::plan(
            command_line(inputs, &inputs.sources),
            prior_state.map(|prior| &prior.inputs),
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &Block<ProtocGenerateInputs>,
        prior_state: Option<&ProtocGenerateState>,
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ProtocGenerateState, ProtocGenerateOutputs>, BoxError> {
        if inputs.out.is_empty() {
            return Err("`out` must name at least one generator".into());
        }
        let compilation = inputs.compilation()?;
        let hashes = {
            let tracker = &*self.tracker;
            inputs.hashes(&compilation, tracker)?
        };

        // The engine reports every change when a field changed, including
        // `dir`, which the prior state does not record. Sources and outputs
        // resolve against `dir`, so the prior run says nothing about them.
        let batches = prior_state
            .filter(|_| !matches!(changes, Changes::All))
            .and_then(|prior| Some((prior, incremental_batches(inputs, prior, &compilation, &hashes)?)));
        let incremental = match batches {
            Some((prior, batches)) => run_incremental(inputs, prior, &batches, compilation.files.len(), writer)?,
            None => {
                writer.starting(&command_line(inputs, &inputs.sources));
                None
            }
        };
        let generated = match incremental {
            Some(generated) => generated,
            None => run_full(inputs, prior_state, &compilation.files, writer)?,
        };

        let outputs = inputs.outputs();
        // Output directories exist even when nothing was generated into them.
        for out in inputs.out.values().filter(|out| !is_archive(out)) {
            let path = super::in_dir(inputs.dir.as_deref(), out);
            std::fs::create_dir_all(&path).map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        }

        Ok(ApplyResult {
            outputs: ProtocGenerateOutputs {
                paths: outputs.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
            },
            state: Some(ProtocGenerateState {
                inputs: inputs.value.clone(),
                compiled: compilation.files,
                protos: hashes.protos,
                tools: hashes.tools,
                generated,
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<ProtocGenerateState>, writer: &BlockWriter) -> Result<(), BoxError> {
        let inputs = Block::new(prior_state.dir.clone(), prior_state.inputs.clone());
        remove_generated(&prior_state.generated, &inputs.outputs(), writer)
    }

    /// Incremental runs compile only the sources that changed.
    fn announces_start(&self) -> bool {
        true
    }

    /// Generators other than protoc's built-in ones are `protoc-gen-*`
    /// executables, and incremental runs depend on the prior local state, so
    /// results stay in this worktree.
    fn cache_policy(&self, _inputs: &Block<ProtocGenerateInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Block<ProtocGenerateInputs>) -> Vec<String> {
        let mut keys = super::removable_outputs(&inputs.outputs()).unwrap_or_default();
        keys.extend(
            inputs
                .descriptor_outputs()
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
        keys
    }
}

/// protoc provider with a `generate` resource.
pub struct ProtocProvider {
    tracker: Arc<FileTracker>,
}

impl ProtocProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Provider for ProtocProvider {
    fn name(&self) -> &str {
        "protoc"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![Box::new(ProtocGenerateResource::new(self.tracker.clone()))]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("protoc provider has no function '{name}'").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn test_resource() -> ProtocGenerateResource {
        ProtocGenerateResource::new(Arc::new(FileTracker::default()))
    }

    /// A project where `proto/acme/v1/a.proto` imports `b.proto`, which
    /// imports `c.proto`, and `d.proto` stands alone.
    fn project() -> (tempfile::TempDir, Block<ProtocGenerateInputs>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto/acme/v1")).unwrap();
        std::fs::write(root.join("proto/acme/v1/a.proto"), r#"import "acme/v1/b.proto";"#).unwrap();
        std::fs::write(root.join("proto/acme/v1/b.proto"), r#"import "acme/v1/c.proto";"#).unwrap();
        std::fs::write(root.join("proto/acme/v1/c.proto"), "").unwrap();
        std::fs::write(root.join("proto/acme/v1/d.proto"), "").unwrap();
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            ProtocGenerateInputs {
                sources: vec!["proto/acme/v1/a.proto".into()],
                include: vec!["proto".into()],
                out: HashMap::from([("go".into(), "gen/go".into()), ("cpp".into(), "gen/cpp".into())]),
                opt: HashMap::from([("go".into(), "paths=source_relative".into())]),
                flags: vec![],
                incremental: false,
            },
        );
        (dir, inputs)
    }

    /// `project()` compiling every file incrementally, with the state a
    /// successful full run would have recorded.
    fn incremental_project() -> (tempfile::TempDir, Block<ProtocGenerateInputs>, ProtocGenerateState) {
        let (dir, mut inputs) = project();
        inputs.sources = vec!["proto/**/*.proto".into()];
        inputs.incremental = true;
        let compilation = inputs.compilation().unwrap();
        let hashes = inputs.hashes(&compilation, &FileTracker::default()).unwrap();
        let prior = ProtocGenerateState {
            inputs: inputs.value.clone(),
            compiled: compilation.files,
            protos: hashes.protos,
            tools: hashes.tools,
            generated: vec![],
        };
        (dir, inputs, prior)
    }

    fn batches(inputs: &Block<ProtocGenerateInputs>, prior: &ProtocGenerateState) -> Option<Batches> {
        let compilation = inputs.compilation().unwrap();
        let hashes = inputs.hashes(&compilation, &FileTracker::default()).unwrap();
        incremental_batches(inputs, prior, &compilation, &hashes)
    }

    #[test]
    fn provider_registration() {
        let provider = ProtocProvider::new(Arc::new(FileTracker::default()));
        assert_eq!(provider.name(), "protoc");
        let resources = provider.resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].name(), "generate");
    }

    #[test]
    fn plan_describes_sorted_generators() {
        let (_dir, inputs) = project();
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(
            result.description,
            "protoc -Iproto --cpp_out=gen/cpp --go_out=gen/go --go_opt=paths=source_relative proto/acme/v1/a.proto"
        );
    }

    #[test]
    fn plan_updates_when_options_change() {
        let (_dir, inputs, mut prior) = incremental_project();
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
        prior.inputs.opt.clear();
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn source_files_expand_globs_relative_to_dir() {
        let (_dir, mut inputs) = project();
        inputs.sources = vec!["proto/**/a.proto".into(), "proto/**/*.proto".into()];
        assert_eq!(
            inputs.source_files().unwrap(),
            [
                "proto/acme/v1/a.proto",
                "proto/acme/v1/b.proto",
                "proto/acme/v1/c.proto",
                "proto/acme/v1/d.proto"
            ]
        );
        inputs.sources = vec!["missing/*.proto".into()];
        assert!(inputs.source_files().is_err());
    }

    #[test]
    fn resolve_tracks_transitive_imports_only() {
        let (_dir, inputs) = project();
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        for name in ["a", "b", "c"] {
            let suffix = format!("acme/v1/{name}.proto");
            assert!(resolved.keys().any(|path| path.ends_with(&suffix)), "{resolved:?}");
        }
        assert!(!resolved.keys().any(|path| path.ends_with("d.proto")), "{resolved:?}");
    }

    #[test]
    fn flag_values_read_every_spelling() {
        let flags: Vec<String> = ["-Ia", "-I", "b", "--proto_path=c:d", "--proto_path", "e", "--opt=x"]
            .map(String::from)
            .into();
        let paths: Vec<_> = flag_values(&flags, "proto_path", Some('I'))
            .into_iter()
            .flat_map(path_list)
            .collect();
        assert_eq!(paths, ["a", "b", "c", "d", "e"]);
        assert!(flag_values(&flags, "descriptor_set_out", Some('o')).is_empty());
    }

    #[test]
    fn flags_and_options_name_inputs_and_outputs() {
        let (dir, mut inputs) = project();
        let root = dir.path();
        std::fs::create_dir_all(root.join("vendor/acme/v1")).unwrap();
        std::fs::write(root.join("vendor/acme/v1/c.proto"), "").unwrap();
        std::fs::remove_file(root.join("proto/acme/v1/c.proto")).unwrap();
        std::fs::write(root.join("deps.binpb"), "").unwrap();
        std::fs::write(root.join("lint.yaml"), "").unwrap();
        inputs.flags = ["-Ivendor", "--descriptor_set_in=deps.binpb", "-oout/set.binpb"]
            .map(String::from)
            .into();
        inputs.opt.insert("doc".into(), "config=lint.yaml,markdown".into());

        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        for name in ["vendor/acme/v1/c.proto", "deps.binpb", "lint.yaml"] {
            assert!(resolved.keys().any(|path| path.ends_with(name)), "{name}: {resolved:?}");
        }
        let keys = Resource::output_keys(&test_resource(), &inputs);
        assert!(keys.iter().any(|key| key.ends_with("out/set.binpb")), "{keys:?}");
    }

    #[test]
    fn tools_skip_builtin_generators_and_honor_plugin_flags() {
        let (_dir, mut inputs) = project();
        inputs.out.insert("grpc".into(), "gen/grpc".into());
        inputs.out.insert("ts".into(), "gen/ts".into());
        inputs.flags = vec![
            "--plugin=protoc-gen-grpc=bin/grpc".into(),
            "--plugin=node_modules/.bin/protoc-gen-ts".into(),
        ];
        let dir = inputs.dir.clone().unwrap();
        assert_eq!(
            inputs.ambient(),
            [
                crate::providers::version("protoc", &["--version"], Some(&dir)),
                Ambient::Executable("protoc-gen-go".into()),
                Ambient::Executable(format!("{dir}/bin/grpc")),
                Ambient::Executable(format!("{dir}/node_modules/.bin/protoc-gen-ts")),
            ]
        );
    }

    #[test]
    fn incremental_compiles_changed_files_and_their_importers() {
        let (dir, inputs, prior) = incremental_project();
        assert_eq!(batches(&inputs, &prior), None, "nothing changed");

        std::fs::write(dir.path().join("proto/acme/v1/c.proto"), "// edited").unwrap();
        let expected = Batches {
            changed: vec![
                "proto/acme/v1/a.proto".into(),
                "proto/acme/v1/b.proto".into(),
                "proto/acme/v1/c.proto".into(),
            ],
            added: vec![],
        };
        assert_eq!(batches(&inputs, &prior), Some(expected));
    }

    #[test]
    fn incremental_compiles_added_sources_separately() {
        let (dir, inputs, prior) = incremental_project();
        std::fs::write(dir.path().join("proto/acme/v1/e.proto"), "").unwrap();
        let expected = Batches {
            changed: vec![],
            added: vec!["proto/acme/v1/e.proto".into()],
        };
        assert_eq!(batches(&inputs, &prior), Some(expected));
    }

    #[test]
    fn incremental_falls_back_to_full_runs() {
        let (dir, inputs, prior) = incremental_project();
        std::fs::write(dir.path().join("proto/acme/v1/d.proto"), "// edited").unwrap();
        assert!(batches(&inputs, &prior).is_some());

        let mut changed_tools = prior.clone();
        changed_tools
            .tools
            .insert("protoc-gen-go".into(), SHA256::digest(b"old"));
        assert_eq!(batches(&inputs, &changed_tools), None, "a tool changed");

        let mut changed_config = prior.clone();
        changed_config.inputs.opt.clear();
        assert_eq!(batches(&inputs, &changed_config), None, "configuration changed");

        let mut not_incremental = inputs.clone();
        not_incremental.incremental = false;
        let mut prior_full = prior.clone();
        prior_full.inputs.incremental = false;
        assert_eq!(batches(&not_incremental, &prior_full), None, "incremental is off");

        let mut archive = prior.clone();
        archive.inputs.out.insert("java".into(), "gen/java.jar".into());
        let mut archive_inputs = inputs.clone();
        archive_inputs.out.insert("java".into(), "gen/java.jar".into());
        assert_eq!(batches(&archive_inputs, &archive), None, "an output is an archive");

        let mut descriptors = prior.clone();
        descriptors.inputs.flags = vec!["--descriptor_set_out=set.binpb".into()];
        let mut descriptor_inputs = inputs.clone();
        descriptor_inputs.flags = descriptors.inputs.flags.clone();
        assert_eq!(
            batches(&descriptor_inputs, &descriptors),
            None,
            "a descriptor set is written"
        );

        std::fs::remove_file(dir.path().join("proto/acme/v1/d.proto")).unwrap();
        assert_eq!(batches(&inputs, &prior), None, "a source was removed");
    }

    #[test]
    fn install_moves_changed_files_and_skips_identical_ones() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir_in(dir.path()).unwrap();
        let out = dir.path().join("gen");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("same.pb.go"), "same").unwrap();
        std::fs::write(staging.path().join("same.pb.go"), "same").unwrap();
        std::fs::write(staging.path().join("new.pb.go"), "new").unwrap();
        let files = vec![
            (staging.path().join("same.pb.go"), out.join("same.pb.go")),
            (staging.path().join("new.pb.go"), out.join("nested/new.pb.go")),
        ];
        let staged = Staged { _dir: staging, files };

        let installed = staged.install().unwrap();
        assert_eq!(installed.len(), 2);
        assert_eq!(std::fs::read_to_string(out.join("same.pb.go")).unwrap(), "same");
        assert_eq!(std::fs::read_to_string(out.join("nested/new.pb.go")).unwrap(), "new");
    }

    #[test]
    fn destroy_removes_only_generated_files_and_empty_directories() {
        let (dir, inputs) = project();
        let out = dir.path().join("gen/go");
        std::fs::create_dir_all(out.join("acme/v1")).unwrap();
        std::fs::write(out.join("acme/v1/a.pb.go"), "package v1").unwrap();
        std::fs::write(out.join("go.mod"), "module example.com/gen").unwrap();
        let state = ProtocGenerateState {
            inputs: inputs.value.clone(),
            compiled: vec![],
            protos: BTreeMap::new(),
            tools: BTreeMap::new(),
            generated: vec![out.join("acme/v1/a.pb.go").to_string_lossy().into_owned()],
        };
        let output = crate::output::Output::new(&[]);
        let state = Block::new(inputs.dir.clone(), state);
        Resource::destroy(&test_resource(), &state, &output.writer("generate")).unwrap();
        assert!(!out.join("acme").exists());
        assert!(out.join("go.mod").exists());
    }
}
