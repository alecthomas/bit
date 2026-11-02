//! Credentials, region, and endpoint, found the way the AWS CLI and SDKs
//! find them, for the subset of their configuration a cache client needs.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// Credentials that sign requests.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Hides the secret parts, so credentials never reach a log.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// Everything a [`crate::Client`] needs to reach S3.
#[derive(Debug, Clone)]
pub struct Config {
    pub credentials: Credentials,
    pub region: String,
    /// An S3-compatible service to use instead of AWS, such as
    /// `https://<account>.r2.cloudflarestorage.com` or a MinIO server.
    pub endpoint: Option<String>,
}

/// Used when no region is configured. S3-compatible services usually
/// accept any region, and AWS redirects nothing for this one.
const DEFAULT_REGION: &str = "us-east-1";

impl Config {
    /// Resolve the configuration from the environment and the AWS shared
    /// configuration files:
    ///
    /// - Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and
    ///   `AWS_SESSION_TOKEN`; otherwise from the profile `AWS_PROFILE` names
    ///   (`default` if unset), as static keys in `~/.aws/credentials` or
    ///   `~/.aws/config`, or from its `credential_process`.
    /// - The region from `AWS_REGION`, `AWS_DEFAULT_REGION`, or the
    ///   profile's `region`, else `us-east-1`.
    /// - The endpoint from `AWS_ENDPOINT_URL_S3`, `AWS_ENDPOINT_URL`, or the
    ///   profile's `endpoint_url`.
    ///
    /// `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` move the files.
    /// Profiles that sign in through SSO or assume a role are not resolved
    /// directly; their error explains how to use them through
    /// `credential_process`.
    pub fn from_env() -> Result<Self, Error> {
        Self::resolve(&|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }

    /// [`Config::from_env`] with variables read through `var`, so tests need
    /// not touch the process environment.
    pub(crate) fn resolve(var: &dyn Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let profile_name = var("AWS_PROFILE").unwrap_or_else(|| "default".to_owned());
        let home = var("HOME").map(PathBuf::from);
        let file = |variable: &str, default: &str| {
            var(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".aws").join(default)))
        };
        let profile = Profile::load(
            &profile_name,
            file("AWS_SHARED_CREDENTIALS_FILE", "credentials").as_deref(),
            file("AWS_CONFIG_FILE", "config").as_deref(),
        )?;

        let credentials = match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => Credentials {
                access_key_id,
                secret_access_key,
                session_token: var("AWS_SESSION_TOKEN"),
            },
            _ => profile.credentials()?,
        };
        let region = var("AWS_REGION")
            .or_else(|| var("AWS_DEFAULT_REGION"))
            .or_else(|| profile.get("region").map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_REGION.to_owned());
        let endpoint = var("AWS_ENDPOINT_URL_S3")
            .or_else(|| var("AWS_ENDPOINT_URL"))
            .or_else(|| profile.get("endpoint_url").map(str::to_owned));
        Ok(Self {
            credentials,
            region,
            endpoint,
        })
    }
}

/// One profile's settings, merged from both files. The credentials file
/// takes precedence, as it does for the AWS CLI.
struct Profile {
    name: String,
    settings: HashMap<String, String>,
}

impl Profile {
    fn load(name: &str, credentials_file: Option<&Path>, config_file: Option<&Path>) -> Result<Self, Error> {
        let mut settings = HashMap::new();
        // The config file names every profile but the default `profile <name>`.
        let config_section = if name == "default" {
            "default".to_owned()
        } else {
            format!("profile {name}")
        };
        for (file, section) in [(config_file, config_section.as_str()), (credentials_file, name)] {
            let Some(file) = file else { continue };
            let contents = match std::fs::read_to_string(file) {
                Ok(contents) => contents,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(Error::ConfigFile {
                        path: file.to_owned(),
                        source,
                    });
                }
            };
            if let Some(found) = parse_ini(&contents).remove(section) {
                settings.extend(found);
            }
        }
        Ok(Self {
            name: name.to_owned(),
            settings,
        })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.settings
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    fn credentials(&self) -> Result<Credentials, Error> {
        if let (Some(access_key_id), Some(secret_access_key)) =
            (self.get("aws_access_key_id"), self.get("aws_secret_access_key"))
        {
            return Ok(Credentials {
                access_key_id: access_key_id.to_owned(),
                secret_access_key: secret_access_key.to_owned(),
                session_token: self.get("aws_session_token").map(str::to_owned),
            });
        }
        if let Some(command) = self.get("credential_process") {
            return credential_process(command);
        }
        const INDIRECT: [&str; 4] = ["sso_session", "sso_start_url", "role_arn", "web_identity_token_file"];
        if let Some(setting) = INDIRECT.iter().find(|setting| self.get(setting).is_some()) {
            return Err(Error::UnsupportedProfile {
                profile: self.name.clone(),
                setting: (*setting).to_owned(),
            });
        }
        Err(Error::NoCredentials {
            profile: self.name.clone(),
        })
    }
}

/// Run a profile's `credential_process` and parse the credentials it
/// prints, in the format the AWS CLI documents.
fn credential_process(command: &str) -> Result<Credentials, Error> {
    let failed = |reason: String| Error::CredentialProcess {
        command: command.to_owned(),
        reason,
    };
    // The AWS CLI splits the command as a shell would, so a shell runs it.
    let output = Command::new("sh")
        .args(["-c", command])
        .output()
        .map_err(|e| failed(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(failed(format!("{}: {}", output.status, stderr.trim())));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| failed(e.to_string()))?;
    let field = |name: &str| json.get(name).and_then(serde_json::Value::as_str).map(str::to_owned);
    match (field("AccessKeyId"), field("SecretAccessKey")) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(Credentials {
            access_key_id,
            secret_access_key,
            session_token: field("SessionToken"),
        }),
        _ => Err(failed("output lacks AccessKeyId or SecretAccessKey".to_owned())),
    }
}

/// Sections of an AWS configuration file, each a map of its top-level
/// settings. Indented lines continue a nested setting, such as an `s3`
/// block, which nothing here reads, so they are skipped.
fn parse_ini(contents: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in contents.lines() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            current = Some(name.split_whitespace().collect::<Vec<_>>().join(" "));
            continue;
        }
        if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            sections
                .entry(section.clone())
                .or_default()
                .insert(key.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home directory with the given AWS files, and variables to resolve
    /// against it.
    struct Fixture {
        home: tempfile::TempDir,
        vars: HashMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".aws")).unwrap();
            let vars = [("HOME".to_owned(), home.path().display().to_string())].into();
            Self { home, vars }
        }

        fn file(self, name: &str, contents: &str) -> Self {
            std::fs::write(self.home.path().join(".aws").join(name), contents).unwrap();
            self
        }

        fn var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_owned(), value.to_owned());
            self
        }

        fn resolve(&self) -> Result<Config, Error> {
            Config::resolve(&|name| self.vars.get(name).cloned())
        }
    }

    fn keys(id: &str, secret: &str) -> Credentials {
        Credentials {
            access_key_id: id.into(),
            secret_access_key: secret.into(),
            session_token: None,
        }
    }

    #[test]
    fn environment_variables_come_first() {
        let config = Fixture::new()
            .file(
                "credentials",
                "[default]\naws_access_key_id = file\naws_secret_access_key = s\n",
            )
            .var("AWS_ACCESS_KEY_ID", "env")
            .var("AWS_SECRET_ACCESS_KEY", "secret")
            .var("AWS_SESSION_TOKEN", "token")
            .var("AWS_REGION", "eu-west-1")
            .resolve()
            .unwrap();
        assert_eq!(
            config.credentials,
            Credentials {
                session_token: Some("token".into()),
                ..keys("env", "secret")
            }
        );
        assert_eq!(config.region, "eu-west-1");
        assert_eq!(config.endpoint, None);
    }

    #[test]
    fn a_named_profile_merges_both_files() {
        let config = Fixture::new()
            .file(
                "credentials",
                "[default]\naws_access_key_id = wrong\naws_secret_access_key = wrong\n\
                 [ci]\naws_access_key_id = id\naws_secret_access_key = secret\n",
            )
            .file(
                "config",
                "# comment\n[default]\nregion = us-west-2\n\
                 [profile ci]\nregion = ap-southeast-2\nendpoint_url = http://localhost:9000\ns3 =\n  addressing_style = path\n",
            )
            .var("AWS_PROFILE", "ci")
            .resolve()
            .unwrap();
        assert_eq!(config.credentials, keys("id", "secret"));
        assert_eq!(config.region, "ap-southeast-2");
        assert_eq!(config.endpoint.as_deref(), Some("http://localhost:9000"));
    }

    #[test]
    fn credential_process_output_is_parsed() {
        let config = Fixture::new()
            .file(
                "config",
                r#"[default]
credential_process = printf '{"Version": 1, "AccessKeyId": "id", "SecretAccessKey": "secret", "SessionToken": "token"}'
"#,
            )
            .resolve()
            .unwrap();
        assert_eq!(config.credentials.session_token.as_deref(), Some("token"));
        assert_eq!(config.region, DEFAULT_REGION);
    }

    #[test]
    fn a_failing_credential_process_is_reported() {
        let err = Fixture::new()
            .file("config", "[default]\ncredential_process = echo expired >&2; exit 3\n")
            .resolve()
            .unwrap_err();
        assert!(
            matches!(&err, Error::CredentialProcess { reason, .. } if reason.contains("expired")),
            "{err}"
        );
    }

    #[test]
    fn sso_profiles_explain_how_to_use_them() {
        let err = Fixture::new()
            .file("config", "[profile dev]\nsso_session = corp\n")
            .var("AWS_PROFILE", "dev")
            .resolve()
            .unwrap_err();
        assert!(matches!(&err, Error::UnsupportedProfile { setting, .. } if setting == "sso_session"));
        assert!(err.to_string().contains("credential_process"), "{err}");
    }

    #[test]
    fn missing_credentials_name_the_profile() {
        let err = Fixture::new().resolve().unwrap_err();
        assert!(matches!(&err, Error::NoCredentials { profile } if profile == "default"));
    }

    #[test]
    fn variables_override_the_endpoint_and_files_can_move() {
        let fixture = Fixture::new();
        let elsewhere = fixture.home.path().join("creds");
        std::fs::write(
            &elsewhere,
            "[default]\naws_access_key_id = moved\naws_secret_access_key = s\n",
        )
        .unwrap();
        let config = fixture
            .var("AWS_SHARED_CREDENTIALS_FILE", &elsewhere.display().to_string())
            .var("AWS_ENDPOINT_URL", "https://generic")
            .var("AWS_ENDPOINT_URL_S3", "https://s3-only")
            .resolve()
            .unwrap();
        assert_eq!(config.credentials.access_key_id, "moved");
        assert_eq!(config.endpoint.as_deref(), Some("https://s3-only"));
    }

    #[test]
    fn debug_output_hides_secrets() {
        let credentials = Credentials {
            session_token: Some("token".into()),
            ..keys("id", "secret")
        };
        let shown = format!("{credentials:?}");
        assert!(
            shown.contains("id") && !shown.contains("secret") && !shown.contains("token"),
            "{shown}"
        );
    }
}
