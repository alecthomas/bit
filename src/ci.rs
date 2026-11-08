//! Split a selection into CI jobs so `large` blocks run apart from the rest.
//!
//! Each job runs `bit` on its own blocks, which also runs everything they
//! depend on, and jobs share no results. A block that needs a large block
//! therefore runs in that large block's job, so the large block is built
//! once rather than again in every job that consumes it.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::dag::{Dag, DagError};
use crate::invocation::Invocation;

/// Job ID for blocks that need no large block.
pub const DEFAULT_JOB: &str = "default";

/// Job ID no block's job takes, so a template can add a job with it that
/// succeeds only when every other job does. Rulesets require checks by name,
/// and the other jobs change with `BUILD.bit`, so this one check is the stable
/// name to require.
pub const GATE_JOB: &str = "ci";

/// One CI job.
#[derive(Debug, PartialEq)]
pub struct Job {
    /// Job identifier, valid as a GitHub Actions job ID.
    pub id: String,
    /// Large blocks this job exists for, in topological order. Empty for the
    /// default job.
    pub large: Vec<String>,
    /// The large blocks as the command line selects them, or `default`.
    pub name: String,
    /// Blocks the job runs, including their dependencies, in topological order.
    pub order: Vec<String>,
    /// Arguments to `bit` that select the job's blocks.
    pub selection: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CiError {
    #[error("{0}")]
    Dag(#[from] DagError),
    #[error("block '{0}' cannot be selected on the command line, so no CI job can run it")]
    Unselectable(String),
    #[error("CI template: {0}")]
    Template(String),
}

/// Split `order`, a selection in topological order, into jobs.
///
/// Large blocks are grouped so that no block needs large blocks from two
/// jobs: a large block that depends on another, or a block that depends on
/// both, puts them in one job. Each group becomes a job that runs its blocks
/// and their dependencies. Blocks that no large job runs form the default
/// job.
pub fn partition(dag: &Dag, order: &[String]) -> Result<Vec<Job>, CiError> {
    // A job runs everything its blocks depend on, even blocks such as
    // `explicit` ones that a bulk selection leaves out, so partition what
    // the jobs will actually run.
    let siblings = siblings(dag);
    let order = closure(&dag.topo_order()?, dag, &siblings, order);
    let order = order.as_slice();
    let mut groups = Groups::default();
    // A representative of the group of large blocks each block needs.
    // Selecting a matrix instance runs all its slices, so a slice needs
    // whatever its siblings need. A sibling can come later in the order,
    // so repeat until no block gains a group.
    let mut needs: HashMap<&str, String> = HashMap::new();
    loop {
        let mut changed = false;
        for name in order {
            let linked = dag
                .deps(name)
                .into_iter()
                .chain(siblings.get(name).cloned().unwrap_or_default());
            let mut members: Vec<String> = linked.filter_map(|other| needs.get(other.as_str()).cloned()).collect();
            if dag.get_node(name).is_some_and(|node| node.large) {
                members.push(name.clone());
            }
            let Some(first) = members.first() else { continue };
            for other in &members[1..] {
                groups.union(first, other);
            }
            if !needs.contains_key(name.as_str()) {
                needs.insert(name, first.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Large blocks and all members of each group, in order of first appearance.
    let mut large_jobs: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut job_of_root: HashMap<String, usize> = HashMap::new();
    for name in order {
        let Some(member) = needs.get(name.as_str()) else {
            continue;
        };
        let root = groups.find(member);
        let index = *job_of_root.entry(root).or_insert_with(|| {
            large_jobs.push((Vec::new(), Vec::new()));
            large_jobs.len() - 1
        });
        let (large, members) = &mut large_jobs[index];
        if dag.get_node(name).is_some_and(|node| node.large) {
            large.push(name.clone());
        }
        members.push(name.clone());
    }

    let mut ids = HashSet::from([DEFAULT_JOB.to_owned(), GATE_JOB.to_owned()]);
    let mut jobs = Vec::new();
    let mut covered = HashSet::new();
    for (large, members) in large_jobs {
        let job_order = closure(order, dag, &siblings, &members);
        covered.extend(job_order.iter().cloned());
        jobs.push(Job {
            id: unique_id(&large[0], &mut ids),
            name: job_name(dag, &job_order, &large),
            large,
            selection: selection(dag, &job_order)?,
            order: job_order,
        });
    }
    let rest: Vec<String> = order.iter().filter(|name| !covered.contains(*name)).cloned().collect();
    if !rest.is_empty() {
        let job_order = closure(order, dag, &siblings, &rest);
        jobs.insert(
            0,
            Job {
                id: DEFAULT_JOB.to_owned(),
                name: DEFAULT_JOB.to_owned(),
                large: Vec::new(),
                selection: selection(dag, &job_order)?,
                order: job_order,
            },
        );
    }
    Ok(jobs)
}

/// For each slice of a matrix instance, the slices that selecting the
/// instance runs, itself included.
fn siblings(dag: &Dag) -> HashMap<String, Vec<String>> {
    let mut by_invocation: HashMap<Vec<String>, Vec<String>> = HashMap::new();
    for name in dag.block_names() {
        if let Some(invocation) = dag.invocation(&name) {
            by_invocation
                .entry(invocation_words(invocation))
                .or_default()
                .push(name);
        }
    }
    by_invocation
        .into_values()
        .filter(|slices| slices.len() > 1)
        .flat_map(|slices| slices.clone().into_iter().map(move |name| (name, slices.clone())))
        .collect()
}

/// `members` and everything selecting them runs, in the order of `order`.
fn closure(order: &[String], dag: &Dag, siblings: &HashMap<String, Vec<String>>, members: &[String]) -> Vec<String> {
    let mut needed: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = members.to_vec();
    while let Some(name) = stack.pop() {
        if needed.insert(name.clone()) {
            stack.extend(dag.deps(&name));
            stack.extend(siblings.get(&name).cloned().unwrap_or_default());
        }
    }
    order.iter().filter(|name| needed.contains(*name)).cloned().collect()
}

fn invocation_words(invocation: &Invocation) -> Vec<String> {
    let mut words = vec![invocation.name.clone()];
    words.extend(invocation.args.iter().map(|(key, value)| format!("{key}={value}")));
    words
}

/// Arguments that make `bit` run exactly `job_order`, a dependency-closed
/// set of blocks: the blocks nothing else in the set depends on.
fn selection(dag: &Dag, job_order: &[String]) -> Result<Vec<String>, CiError> {
    let selectors = Selectors::new(dag, job_order);
    let mut words = Vec::new();
    let mut selected = HashSet::new();
    for name in job_order {
        if dag
            .dependents(name)
            .iter()
            .any(|dependent| selectors.in_job.contains(dependent.as_str()))
        {
            continue;
        }
        let selector = selectors.get(name).ok_or_else(|| CiError::Unselectable(name.clone()))?;
        if selected.insert(selector.clone()) {
            words.extend(selector);
        }
    }
    Ok(words)
}

/// The job's large blocks as the command line selects them, or
/// [`DEFAULT_JOB`].
fn job_name(dag: &Dag, job_order: &[String], large: &[String]) -> String {
    if large.is_empty() {
        return DEFAULT_JOB.to_owned();
    }
    let selectors = Selectors::new(dag, job_order);
    let mut names: Vec<String> = Vec::new();
    for block in large {
        let name = selectors
            .get(block)
            .map_or_else(|| block.clone(), |selector| shell_join(&selector));
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.join(", ")
}

/// Finds the command-line words that select a block within one job.
struct Selectors<'a> {
    dag: &'a Dag,
    in_job: HashSet<&'a str>,
    matrices: HashMap<String, Vec<String>>,
}

impl<'a> Selectors<'a> {
    fn new(dag: &'a Dag, job_order: &'a [String]) -> Self {
        Self {
            dag,
            in_job: job_order.iter().map(String::as_str).collect(),
            matrices: matrix_slices(dag),
        }
    }

    /// The words that select `name`, or `None` if the command line cannot.
    fn get(&self, name: &str) -> Option<Vec<String>> {
        if let Some(invocation) = self.dag.invocation(name) {
            Some(invocation_words(invocation))
        } else if let Some(matrix) = whole_matrix(self.dag, &self.matrices, &self.in_job, name) {
            Some(vec![matrix.to_owned()])
        } else if name.contains(['(', '=']) {
            // The command line reads `=` as a target argument, and only a
            // recorded invocation can recreate an instance.
            None
        } else {
            Some(vec![name.to_owned()])
        }
    }
}

/// The slices of each matrix block, keyed by the block's name.
///
/// Only matrix expansion gives an uninvoked block a foreign concurrency group.
fn matrix_slices(dag: &Dag) -> HashMap<String, Vec<String>> {
    let mut matrices: HashMap<String, Vec<String>> = HashMap::new();
    for name in dag.block_names() {
        let Some(node) = dag.get_node(&name) else { continue };
        if node.concurrency_group != name && dag.invocation(&name).is_none() {
            matrices.entry(node.concurrency_group.clone()).or_default().push(name);
        }
    }
    matrices
}

/// The name of the matrix block that `slice` belongs to, if the job runs all
/// of that block's slices, so naming the block selects nothing extra.
fn whole_matrix<'a>(
    dag: &'a Dag,
    matrices: &HashMap<String, Vec<String>>,
    in_job: &HashSet<&str>,
    slice: &str,
) -> Option<&'a str> {
    let matrix = dag.get_node(slice)?.concurrency_group.as_str();
    let slices = matrices.get(matrix)?;
    slices
        .iter()
        .all(|slice| in_job.contains(slice.as_str()))
        .then_some(matrix)
}

/// A GitHub Actions job ID derived from a block name, which may contain
/// matrix keys or instance arguments that job IDs do not allow.
fn unique_id(block: &str, taken: &mut HashSet<String>) -> String {
    let mut base = String::new();
    for c in block.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            base.push(c);
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let mut base = base.trim_matches('-').to_owned();
    if !base.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        base.insert(0, '_');
    }
    let mut id = base.clone();
    let mut suffix = 2;
    while !taken.insert(id.clone()) {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

/// Union-find over large block names.
#[derive(Default)]
struct Groups {
    parent: HashMap<String, String>,
}

impl Groups {
    fn find(&mut self, name: &str) -> String {
        let mut root = name.to_owned();
        while let Some(parent) = self.parent.get(&root) {
            root = parent.clone();
        }
        root
    }

    fn union(&mut self, left: &str, right: &str) {
        let (left, right) = (self.find(left), self.find(right));
        if left != right {
            self.parent.insert(right, left);
        }
    }
}

/// Templates built into bit, by the name `--ci` takes.
const BUILTIN_TEMPLATES: [(&str, &str); 1] = [("github", include_str!("ci/github.yml"))];

/// The source of the built-in template `name`.
pub fn builtin_template(name: &str) -> Option<&'static str> {
    BUILTIN_TEMPLATES
        .iter()
        .find(|(builtin, _)| *builtin == name)
        .map(|(_, source)| *source)
}

/// Names of the built-in templates.
pub fn builtin_template_names() -> impl Iterator<Item = &'static str> {
    BUILTIN_TEMPLATES.iter().map(|(name, _)| *name)
}

/// What a template sees besides the jobs.
pub struct TemplateContext<'a> {
    /// The `bit --ci` command that generated the file.
    pub command: &'a [String],
    /// The project directory relative to the repository root, if not the root.
    pub working_directory: Option<&'a str>,
    /// The remote's default branch, if known.
    pub default_branch: Option<&'a str>,
}

/// A job as templates see it. Its fields are the contract with
/// user-written templates, so renaming one breaks them.
#[derive(Serialize)]
struct TemplateJob<'a> {
    id: &'a str,
    name: &'a str,
    large: bool,
    command: String,
    selection: &'a [String],
}

/// Render `jobs` with `source`, a minijinja template named `name` in errors.
pub fn render(name: &str, source: &str, jobs: &[Job], context: &TemplateContext) -> Result<String, CiError> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    // The built-in `tojson` escapes `'`, `<`, `>` and `&` for HTML, which
    // YAML and JSON don't need and which makes commands unreadable.
    env.add_filter("tojson", |value: minijinja::Value| {
        serde_json::to_string(&value)
            .map_err(|e| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string()))
    });
    // Block tags sit on their own lines in YAML templates; drop those lines.
    let syntax = minijinja::syntax::SyntaxConfig::builder()
        .trim_blocks(true)
        .lstrip_blocks(true)
        .keep_trailing_newline(true)
        .build()
        .map_err(|e| CiError::Template(e.to_string()))?;
    env.set_syntax(syntax);
    let jobs: Vec<_> = jobs
        .iter()
        .map(|job| {
            let mut command = vec!["bit".to_owned()];
            command.extend(job.selection.iter().cloned());
            TemplateJob {
                id: &job.id,
                name: &job.name,
                large: !job.large.is_empty(),
                command: shell_join(&command),
                selection: &job.selection,
            }
        })
        .collect();
    let template = env
        .template_from_named_str(name, source)
        .map_err(|e| CiError::Template(e.to_string()))?;
    template
        .render(minijinja::context! {
            jobs => minijinja::value::Serde(&jobs),
            command => shell_join(context.command),
            working_directory => context.working_directory,
            default_branch => context.default_branch,
        })
        .map_err(|e| CiError::Template(e.to_string()))
}

/// Join words into a shell command, quoting only what needs it. A
/// `name=value` argument quotes just its value, as a person would write it.
fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|word| match word.split_once('=') {
            Some((name, value)) if is_argument_name(name) => format!("{name}={}", shell_quote(value)),
            _ => shell_quote(word),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_argument_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:@%+,=".contains(c));
    if safe {
        return word.to_owned();
    }
    shlex::try_quote(word).map_or_else(|_| word.to_owned(), |quoted| quoted.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tracker::FileTracker;
    use crate::invocation::Invocation;
    use crate::parser;
    use crate::provider::ProviderRegistry;
    use crate::providers::exec::ExecProvider;
    use crate::state::{StateError, StateStore};
    use crate::value::Map;
    use std::sync::Arc;

    struct EmptyStore;
    impl StateStore for EmptyStore {
        fn load(&self, _block: &str) -> Result<Option<serde_json::Value>, StateError> {
            Ok(None)
        }
        fn save(&self, _block: &str, _state: &serde_json::Value) -> Result<(), StateError> {
            Ok(())
        }
        fn remove(&self, _block: &str) -> Result<(), StateError> {
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StateError> {
            Ok(vec![])
        }
    }

    fn jobs_for(input: &str, selector: &str, args: &[(&str, &str)]) -> Result<Vec<Job>, CiError> {
        let module = parser::parse(input, "<test>").unwrap();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(ExecProvider::new(Arc::new(FileTracker::new()))));
        let invocations = if selector.is_empty() {
            Vec::new()
        } else {
            vec![Invocation {
                name: selector.into(),
                args: args
                    .iter()
                    .map(|(key, value)| ((*key).into(), (*value).into()))
                    .collect(),
            }]
        };
        let (dag, _base, targets) =
            crate::loader::load_selected(&module, &Map::new(), &registry, &EmptyStore, &[], &invocations).unwrap();
        let order = crate::engine::resolve_order(&dag, &targets).unwrap();
        partition(&dag, &order)
    }

    fn jobs(input: &str) -> Vec<Job> {
        jobs_for(input, "", &[]).unwrap()
    }

    fn summary(jobs: &[Job]) -> Vec<(&str, Vec<&str>, Vec<&str>)> {
        jobs.iter()
            .map(|job| {
                (
                    job.id.as_str(),
                    job.order.iter().map(String::as_str).collect(),
                    job.selection.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn without_large_blocks_everything_runs_in_the_default_job() {
        let jobs = jobs(
            r#"
a = exec { command = "a" }
b = exec { command = "b" depends_on = [a] }
"#,
        );
        assert_eq!(summary(&jobs), vec![("default", vec!["a", "b"], vec!["b"])]);
    }

    #[test]
    fn large_blocks_run_in_their_own_jobs_with_their_consumers() {
        let jobs = jobs(
            r#"
gen = exec { command = "gen" }
large build = exec { command = "build" depends_on = [gen] }
large image = exec { command = "image" }
check = exec { command = "check" depends_on = [build] }
lint = exec { command = "lint" }
"#,
        );
        // gen runs only where build needs it, not again in the default job.
        assert_eq!(
            summary(&jobs),
            vec![
                ("default", vec!["lint"], vec!["lint"]),
                ("build", vec!["gen", "build", "check"], vec!["check"]),
                ("image", vec!["image"], vec!["image"]),
            ]
        );
        assert_eq!(jobs[1].large, vec!["build"]);
    }

    #[test]
    fn blocks_needing_two_large_blocks_join_their_jobs() {
        let jobs = jobs(
            r#"
large a = exec { command = "a" }
large b = exec { command = "b" }
large c = exec { command = "c" after = [a] }
large d = exec { command = "d" }
both = exec { command = "both" depends_on = [b, c] }
"#,
        );
        assert_eq!(
            summary(&jobs),
            vec![
                ("a", vec!["a", "b", "c", "both"], vec!["both"]),
                ("d", vec!["d"], vec!["d"])
            ]
        );
        assert_eq!(jobs[0].large, vec!["a", "b", "c"]);
    }

    #[test]
    fn pre_blocks_run_in_every_job() {
        let jobs = jobs(
            r#"
pre fmt = exec { command = "fmt" }
large build = exec { command = "build" }
lint = exec { command = "lint" }
"#,
        );
        assert_eq!(
            summary(&jobs),
            vec![
                ("default", vec!["fmt", "lint"], vec!["lint"]),
                ("build", vec!["fmt", "build"], vec!["build"])
            ]
        );
    }

    #[test]
    fn job_ids_are_valid_and_distinct() {
        let jobs = jobs(
            r#"
let arch = ["amd64", "arm64"]
large build[arch] = exec { command = "build #{arch}" }
large default = exec { command = "default" }
large ci = exec { command = "ci" }
"#,
        );
        let ids: Vec<_> = jobs.iter().map(|job| job.id.as_str()).collect();
        assert_eq!(ids, vec!["build-amd64", "build-arm64", "ci-2", "default-2"]);
        assert_eq!(jobs[0].selection, vec![r#"build["amd64"]"#]);
    }

    #[test]
    fn selection_limits_the_jobs() {
        let jobs = jobs_for(
            r#"
large build = exec { command = "build" }
large image = exec { command = "image" }
target release = [build]
"#,
            "release",
            &[],
        )
        .unwrap();
        assert_eq!(summary(&jobs), vec![("build", vec!["build"], vec!["build"])]);
    }

    #[test]
    fn jobs_run_explicit_dependencies() {
        let jobs = jobs(
            r#"
explicit gen = exec { command = "gen" }
build = exec { command = "build" depends_on = [gen] }
"#,
        );
        assert_eq!(summary(&jobs), vec![("default", vec!["gen", "build"], vec!["build"])]);
    }

    #[test]
    fn instances_are_selected_by_their_invocation() {
        let jobs = jobs_for(
            r#"
large publish(name : string, count = 1) = exec { command = "publish #{name} #{count}" }
"#,
            "publish",
            &[("name", "web app"), ("count", "2")],
        )
        .unwrap();
        assert_eq!(jobs[0].selection, vec!["publish", "name=web app", "count=2"]);
    }

    #[test]
    fn instances_leave_out_default_arguments() {
        let jobs = jobs_for(
            r#"
let arch = ["amd64", "arm64"]
large test[arch](package = "./...", run = ".", tags = package) = exec { command = "test #{package} #{run} #{tags}" }
"#,
            "test",
            &[("run", "x"), ("tags", "./...")],
        )
        .unwrap();
        assert_eq!(jobs[0].selection, vec!["test", "run=x"]);
    }

    #[test]
    fn matrix_instance_slices_share_a_job() {
        let jobs = jobs_for(
            r#"
let arch = ["amd64", "arm64"]
large build[arch](name : string) = exec { command = "build #{name} #{arch}" }
"#,
            "build",
            &[("name", "app")],
        )
        .unwrap();
        assert_eq!(
            summary(&jobs),
            vec![(
                "build-amd64-name-app",
                vec![r#"build["amd64"](name="app")"#, r#"build["arm64"](name="app")"#],
                vec!["build", "name=app"]
            )]
        );
        assert_eq!(jobs[0].name, "build name=app");
    }

    #[test]
    fn job_names_select_their_large_blocks() {
        let jobs = jobs(
            r#"
let arch = ["amd64", "arm64"]
large build[arch] = exec { command = "build #{arch}" }
large image = exec { command = "image" depends_on = [build] }
lint = exec { command = "lint" }
"#,
        );
        let names: Vec<_> = jobs.iter().map(|job| job.name.as_str()).collect();
        assert_eq!(names, vec!["default", "build, image"]);
    }

    #[test]
    fn whole_matrices_are_selected_by_name() {
        let jobs = jobs(
            r#"
let arch = ["amd64", "arm64"]
build[arch] = exec { command = "build #{arch}" }
lint = exec { command = "lint" }
"#,
        );
        assert_eq!(jobs[0].selection, vec!["build", "lint"]);
    }

    #[test]
    fn matrix_slices_split_across_jobs_are_selected_alone() {
        let jobs = jobs(
            r#"
let arch = ["amd64", "arm64"]
large build[arch] = exec { command = "build #{arch}" }
"#,
        );
        assert_eq!(
            summary(&jobs),
            vec![
                ("build-amd64", vec![r#"build["amd64"]"#], vec![r#"build["amd64"]"#]),
                ("build-arm64", vec![r#"build["arm64"]"#], vec![r#"build["arm64"]"#]),
            ]
        );
    }

    #[test]
    fn workflow_runs_bit_on_each_job_selection() {
        let jobs = vec![
            Job {
                id: "default".into(),
                name: "default".into(),
                large: vec![],
                order: vec!["lint".into()],
                selection: vec!["lint".into()],
            },
            Job {
                id: "publish".into(),
                name: "publish name='web app'".into(),
                large: vec![r#"publish(name="web app")"#.into()],
                order: vec![r#"publish(name="web app")"#.into()],
                selection: vec!["publish".into(), "name=web app".into()],
            },
        ];
        let command = words(&["bit", "--ci", "github", "publish", "name=web app"]);
        let yaml = github(
            &jobs,
            &TemplateContext {
                command: &command,
                working_directory: Some("app"),
                default_branch: Some("main"),
            },
        );
        assert!(yaml.starts_with("# Generated by `bit --ci github publish name='web app'`. Do not edit.\n"));
        assert!(yaml.contains(r#"- run: "bit publish name='web app'""#), "{yaml}");
        let workflow: serde_yml::Value = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(workflow["on"]["push"]["branches"][0], "main");
        assert!(workflow["on"]["pull_request"].is_null());
        let job = &workflow["jobs"]["publish"];
        assert_eq!(job["name"], "publish name='web app'");
        assert_eq!(job["runs-on"], "ubuntu-latest");
        assert_eq!(job["steps"][0]["uses"], "actions/checkout@v6");
        assert_eq!(job["steps"][1]["run"], "bit publish name='web app'");
        assert_eq!(job["steps"][1]["working-directory"], "app");
        assert_eq!(workflow["jobs"]["default"]["name"], "default");
        let gate = &workflow["jobs"]["ci"];
        assert_eq!(gate["name"], "ci");
        assert_eq!(gate["if"], "always()");
        assert_eq!(gate["needs"], serde_yml::to_value(["default", "publish"]).unwrap());
        assert_eq!(gate["steps"][0]["run"], "exit 1");
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn github(jobs: &[Job], context: &TemplateContext) -> String {
        render("github", builtin_template("github").unwrap(), jobs, context).unwrap()
    }

    #[test]
    fn templates_see_each_job() {
        let jobs = vec![Job {
            id: "build".into(),
            name: "build".into(),
            large: vec!["build".into()],
            order: vec!["build".into()],
            selection: words(&["build", "name=web app"]),
        }];
        let command = words(&["bit", "--ci", "./ci.tmpl"]);
        let context = TemplateContext {
            command: &command,
            working_directory: None,
            default_branch: None,
        };
        let source = "# {{ command }}\n{% for job in jobs %}\n{{ job.id }} {{ job.name }} {{ job.large | tojson }} {{ job.selection | tojson }}: {{ job.command }}\n{% endfor %}\n";
        assert_eq!(
            render("ci.tmpl", source, &jobs, &context).unwrap(),
            "# bit --ci ./ci.tmpl\nbuild build true [\"build\",\"name=web app\"]: bit build name='web app'\n"
        );
    }

    #[test]
    fn templates_reject_undefined_values() {
        let command = words(&["bit"]);
        let context = TemplateContext {
            command: &command,
            working_directory: None,
            default_branch: None,
        };
        let error = render("ci.tmpl", "{{ runner }}", &[], &context).unwrap_err();
        assert!(error.to_string().contains("ci.tmpl"), "{error}");
    }

    #[test]
    fn commands_quote_only_what_the_shell_needs() {
        let words = words(&[
            "bit",
            "build",
            "name=app",
            "tag=",
            "label=a b",
            r#"build["amd64"]"#,
            "x y=z",
        ]);
        assert_eq!(
            shell_join(&words),
            r#"bit build name=app tag='' label='a b' 'build["amd64"]' 'x y=z'"#
        );
    }

    #[test]
    fn workflow_without_branch_or_working_directory() {
        let jobs = vec![Job {
            id: "default".into(),
            name: "default".into(),
            large: vec![],
            order: vec!["a".into()],
            selection: vec!["a".into()],
        }];
        let command = words(&["bit", "--ci", "github"]);
        let yaml = github(
            &jobs,
            &TemplateContext {
                command: &command,
                working_directory: None,
                default_branch: None,
            },
        );
        let workflow: serde_yml::Value = serde_yml::from_str(&yaml).unwrap();
        assert!(workflow["on"]["push"].is_null());
        let steps = workflow["jobs"]["default"]["steps"].as_sequence().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["run"], "bit a");
        assert!(steps[1].get("working-directory").is_none());
    }
}
