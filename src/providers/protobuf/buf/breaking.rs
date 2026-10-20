use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, ApplyResult, BoxError, CachePolicy, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::BufTarget;

/// Check Protobuf files for breaking changes with buf breaking
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufBreakingInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Baseline input to compare against (e.g. ".git#branch=main")
    pub against: String,
    /// Extra flags passed to buf breaking
    #[serde(default)]
    pub flags: Vec<String>,
}

impl BufBreakingInputs {
    fn args(&self) -> Vec<String> {
        let mut args = vec!["breaking".to_owned(), "--against".to_owned(), self.against.clone()];
        args.extend(self.flags.iter().cloned());
        args
    }
}

/// Outputs from a `buf.breaking` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct BufBreakingOutputs {
    /// Whether no breaking changes were found
    pub passed: bool,
}

/// Persisted state for a `buf.breaking` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufBreakingState {
    #[serde(flatten)]
    pub inputs: BufBreakingInputs,
    /// Commit a local Git baseline resolved to when the check ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub against_commit: Option<String>,
}

/// The commit a baseline in the current repository names, so a check
/// re-runs when its branch moves. `None` for any other baseline.
///
/// Buf reads `<path>.git#branch=…,tag=…,ref=…` by cloning the local
/// repository, so a branch or tag means the local `refs/heads` or
/// `refs/tags` entry and no option means `HEAD`.
fn git_baseline_commit(against: &str, dir: Option<&str>) -> Result<Option<String>, BoxError> {
    let (path, options) = against.split_once('#').unwrap_or((against, ""));
    // A URL or `user@host:path` names a remote repository.
    if !path.ends_with(".git") || path.contains(':') {
        return Ok(None);
    }
    let mut rev = "HEAD".to_owned();
    for option in options.split(',') {
        match option.split_once('=') {
            Some(("branch", branch)) => rev = format!("refs/heads/{branch}"),
            Some(("tag", tag)) => rev = format!("refs/tags/{tag}"),
            Some(("ref", reference)) => rev = reference.to_owned(),
            _ => {}
        }
    }

    // The path names the Git directory itself, which may be a bare repository.
    let repo = super::super::in_dir(dir, path);
    let output = crate::git::command()
        .arg("--git-dir")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .map_err(|e| format!("failed to execute `git rev-parse`: {e}"))?;
    if !output.status.success() {
        return Err(format!("baseline `{against}` does not name a commit in {}", repo.display()).into());
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

pub struct BufBreakingResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl BufBreakingResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Resource for BufBreakingResource {
    type State = BufBreakingState;
    type Inputs = BufBreakingInputs;
    type Outputs = BufBreakingOutputs;

    fn name(&self) -> &str {
        "breaking"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    /// A baseline that is a local directory is tracked like the input.
    fn resolve(&self, inputs: &BufBreakingInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        let mut hashes = inputs.target.resolve(&[], &mut tracker)?;
        let baseline = super::super::in_dir(inputs.target.dir.as_deref(), &inputs.against);
        if baseline.is_dir() {
            hashes.extend(super::super::hash_protos(&[baseline], &mut tracker)?);
        }
        Ok(hashes)
    }

    fn ambient(&self, inputs: &BufBreakingInputs) -> Vec<Ambient> {
        inputs.target.check_ambient()
    }

    fn plan(&self, inputs: &BufBreakingInputs, prior_state: Option<&BufBreakingState>) -> Result<PlanResult, BoxError> {
        let mut result = super::super::plan(
            inputs.target.describe(&inputs.args()),
            prior_state.map(|prior| &prior.inputs),
            inputs,
        );
        if let Some(prior) = prior_state
            && result.action == PlanAction::None
            && git_baseline_commit(&inputs.against, inputs.target.dir.as_deref())? != prior.against_commit
        {
            result.action = PlanAction::Update;
            result.reason = Some("baseline moved".to_owned());
        }
        Ok(result)
    }

    fn apply(
        &self,
        inputs: &BufBreakingInputs,
        _prior_state: Option<&BufBreakingState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufBreakingState, BufBreakingOutputs>, BoxError> {
        let against_commit = git_baseline_commit(&inputs.against, inputs.target.dir.as_deref())?;
        let status = inputs.target.run(&inputs.args(), writer)?;
        Ok(ApplyResult {
            outputs: BufBreakingOutputs {
                passed: status.success(),
            },
            state: Some(BufBreakingState {
                inputs: inputs.clone(),
                against_commit,
            }),
        })
    }

    fn destroy(&self, _prior_state: &BufBreakingState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The baseline usually lives outside the tracked sources, as a Git ref
    /// or a remote module, so the action key cannot describe it.
    fn cache_policy(&self, _inputs: &BufBreakingInputs) -> CachePolicy {
        CachePolicy::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> BufBreakingResource {
        BufBreakingResource::new(Arc::new(Mutex::new(FileTracker::default())))
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = crate::git::command()
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
        dir
    }

    #[test]
    fn baseline_commit_follows_branch_tag_and_head() {
        let dir = repo();
        let root = dir.path().to_string_lossy().into_owned();
        let first = git(dir.path(), &["rev-parse", "HEAD"]);
        git(dir.path(), &["tag", "v1"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
        let second = git(dir.path(), &["rev-parse", "HEAD"]);

        assert_eq!(
            git_baseline_commit(".git#branch=main", Some(&root)).unwrap(),
            Some(second.clone())
        );
        assert_eq!(git_baseline_commit(".git#tag=v1", Some(&root)).unwrap(), Some(first));
        assert_eq!(git_baseline_commit(".git", Some(&root)).unwrap(), Some(second));
    }

    #[test]
    fn baseline_commit_ignores_other_baselines() {
        assert_eq!(git_baseline_commit("buf.build/acme/api", None).unwrap(), None);
        assert_eq!(
            git_baseline_commit("https://github.com/acme/api.git#branch=main", None).unwrap(),
            None
        );
        assert_eq!(git_baseline_commit("git@github.com:acme/api.git", None).unwrap(), None);
        assert_eq!(git_baseline_commit("../baseline", None).unwrap(), None);
    }

    #[test]
    fn baseline_commit_rejects_unknown_branch() {
        let dir = repo();
        let root = dir.path().to_string_lossy().into_owned();
        assert!(git_baseline_commit(".git#branch=missing", Some(&root)).is_err());
    }

    #[test]
    fn plan_updates_when_baseline_moves() {
        let dir = repo();
        let inputs = BufBreakingInputs {
            target: BufTarget {
                input: None,
                dir: Some(dir.path().to_string_lossy().into_owned()),
            },
            against: ".git#branch=main".into(),
            flags: vec![],
        };
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        let prior = BufBreakingState {
            inputs: inputs.clone(),
            against_commit: Some(head),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
        assert_eq!(result.description, "buf breaking --against .git#branch=main");

        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
