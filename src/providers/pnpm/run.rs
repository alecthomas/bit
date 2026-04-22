use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::{BlockWriter, Event};
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::{run_pnpm, workspace};

/// Deserialize a field as either a single string or a list of strings.
/// Matches the ergonomics of the `exec` provider's `output` field.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct V;

    impl<'de> de::Visitor<'de> for V {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or list of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element()? {
                out.push(s);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(V)
}

fn default_dir() -> String {
    ".".to_owned()
}

/// Build a list of pnpm command arguments from resource inputs.
fn pnpm_args(script: &str, package: Option<&str>, script_args: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(pkg) = package {
        args.push("--filter".to_owned());
        args.push(pkg.to_owned());
    }
    args.push("run".to_owned());
    args.push(script.to_owned());
    if !script_args.is_empty() {
        args.push("--".to_owned());
        args.extend(script_args.iter().cloned());
    }
    args
}

/// Render a shell-ish description of the `pnpm` invocation (for plan output).
fn describe(script: &str, package: Option<&str>, script_args: &[String]) -> String {
    let mut parts = vec!["pnpm".to_owned()];
    if let Some(pkg) = package {
        parts.push("--filter".to_owned());
        parts.push(pkg.to_owned());
    }
    parts.push("run".to_owned());
    parts.push(script.to_owned());
    if !script_args.is_empty() {
        parts.push("--".to_owned());
        parts.extend(script_args.iter().cloned());
    }
    parts.join(" ")
}

/// Resolve input files for a `pnpm.run` or `pnpm.test` block: auto-scan the
/// target package's source tree (recursively, excluding common build
/// directories and declared outputs) plus the workspace lockfile. When no
/// `package` is given, only the root `package.json` and lockfile are
/// included — root-level scripts rarely need the whole tree hashed.
pub(super) fn resolve_inputs(
    dir: &str,
    package: Option<&str>,
    output: &[String],
    extra_globs: &[String],
) -> Result<Vec<ResolvedFile>, BoxError> {
    workspace::with_workspace(Path::new(dir), |ws| {
        let mut files: Vec<ResolvedFile> = Vec::new();

        if let Some(lf) = &ws.lockfile {
            files.push(ResolvedFile::Input(lf.clone()));
        }

        match package {
            Some(pkg) => {
                let pkg_dir = ws
                    .package_dir(pkg)
                    .ok_or_else(|| format!("pnpm: package '{pkg}' not found in workspace"))?
                    .to_path_buf();
                let exclude_outputs: Vec<PathBuf> = output.iter().map(PathBuf::from).collect();
                for f in workspace::scan_sources(&pkg_dir, &exclude_outputs) {
                    files.push(ResolvedFile::Input(f));
                }
            }
            None => {
                files.push(ResolvedFile::Input(ws.root_package_json.clone()));
            }
        }

        for glob in extra_globs {
            files.push(ResolvedFile::InputGlob(glob.clone()));
        }
        for path in output {
            files.push(ResolvedFile::Output(PathBuf::from(path)));
        }

        Ok(files)
    })
}

/// Run a script defined in `package.json`.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmRunInputs {
    /// Script name from `package.json` (e.g. "build")
    pub script: String,
    /// Package name from its `package.json`. Omit to run at the workspace root.
    #[serde(default)]
    pub package: Option<String>,
    /// Additional arguments passed to the script after `--`
    #[serde(default)]
    pub args: Vec<String>,
    /// Output file or list of output files/directories produced by the script
    #[serde(default, deserialize_with = "string_or_vec")]
    pub output: Vec<String>,
    /// Extra input file globs (added to auto-detected sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Workspace root directory (defaults to the current directory)
    #[serde(default = "default_dir")]
    pub dir: String,
}

/// Outputs from a `pnpm.run` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct PnpmRunOutputs {
    /// Single output path, when exactly one was declared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Multiple output paths, when more than one was declared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl PnpmRunOutputs {
    pub(super) fn from_paths(output: &[String]) -> Self {
        match output.len() {
            0 => Self {
                path: None,
                paths: None,
            },
            1 => Self {
                path: Some(output[0].clone()),
                paths: None,
            },
            _ => Self {
                path: None,
                paths: Some(output.to_vec()),
            },
        }
    }
}

/// Persisted state for a `pnpm.run` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnpmRunState {
    pub script: String,
    pub package: Option<String>,
    pub args: Vec<String>,
    pub output: Vec<String>,
    pub dir: String,
}

pub struct PnpmRunResource;

impl Resource for PnpmRunResource {
    type State = PnpmRunState;
    type Inputs = PnpmRunInputs;
    type Outputs = PnpmRunOutputs;

    fn name(&self) -> &str {
        "run"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &PnpmRunInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        resolve_inputs(&inputs.dir, inputs.package.as_deref(), &inputs.output, &inputs.inputs)
    }

    fn plan(&self, inputs: &PnpmRunInputs, prior_state: Option<&PnpmRunState>) -> Result<PlanResult, BoxError> {
        let description = describe(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };
        let action = if prior.script != inputs.script
            || prior.package != inputs.package
            || prior.args != inputs.args
            || prior.output != inputs.output
            || prior.dir != inputs.dir
        {
            PlanAction::Update
        } else {
            PlanAction::None
        };
        Ok(PlanResult {
            action,
            description,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &PnpmRunInputs,
        _prior_state: Option<&PnpmRunState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmRunState, PnpmRunOutputs>, BoxError> {
        for output in &inputs.output {
            let p = Path::new(output);
            if output.ends_with('/') {
                fs::create_dir_all(p)?;
            } else if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
        }

        let args = pnpm_args(&inputs.script, inputs.package.as_deref(), &inputs.args);
        run_pnpm(&args, Some(&inputs.dir), writer)?;

        Ok(ApplyResult {
            outputs: PnpmRunOutputs::from_paths(&inputs.output),
            state: Some(PnpmRunState {
                script: inputs.script.clone(),
                package: inputs.package.clone(),
                args: inputs.args.clone(),
                output: inputs.output.clone(),
                dir: inputs.dir.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &PnpmRunState, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.output {
            let path = Path::new(output);
            if path.is_dir() {
                writer.event(Event::Starting, &format!("rm -rf {output}"));
                fs::remove_dir_all(path).ok();
            } else if path.is_file() {
                writer.event(Event::Starting, &format!("rm {output}"));
                fs::remove_file(path).ok();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&PnpmRunResource), ResourceKind::Build);
    }

    #[test]
    fn build_args_without_package() {
        let args = pnpm_args("build", None, &[]);
        assert_eq!(args, vec!["run", "build"]);
    }

    #[test]
    fn build_args_with_package_and_script_args() {
        let args = pnpm_args("build", Some("bff"), &["--mode=production".to_owned()]);
        assert_eq!(args, vec!["--filter", "bff", "run", "build", "--", "--mode=production"]);
    }

    #[test]
    fn describe_matches_args() {
        assert_eq!(describe("build", Some("bff"), &[]), "pnpm --filter bff run build");
        assert_eq!(
            describe("build", None, &["--watch".to_owned()]),
            "pnpm run build -- --watch"
        );
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = PnpmRunInputs {
            script: "build".into(),
            package: Some("bff".into()),
            args: vec![],
            output: vec![],
            inputs: vec![],
            dir: ".".into(),
        };
        let result = Resource::plan(&PnpmRunResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_script_changed() {
        let inputs = PnpmRunInputs {
            script: "test".into(),
            package: Some("bff".into()),
            args: vec![],
            output: vec![],
            inputs: vec![],
            dir: ".".into(),
        };
        let prior = PnpmRunState {
            script: "build".into(),
            package: Some("bff".into()),
            args: vec![],
            output: vec![],
            dir: ".".into(),
        };
        let result = Resource::plan(&PnpmRunResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn outputs_single_vs_multiple() {
        let o = PnpmRunOutputs::from_paths(&["bff/dist".to_owned()]);
        assert_eq!(o.path.as_deref(), Some("bff/dist"));
        assert!(o.paths.is_none());
        let o = PnpmRunOutputs::from_paths(&["a".to_owned(), "b".to_owned()]);
        assert!(o.path.is_none());
        assert_eq!(o.paths, Some(vec!["a".into(), "b".into()]));
    }
}
