//! Maven coordinates and the repository layout they map to.

use std::fmt;
use std::str::FromStr;

use crate::provider::BoxError;

/// An artifact apart from its version: what Maven treats as one dependency
/// when choosing between versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::providers::java) struct Key {
    pub group: String,
    pub artifact: String,
    pub extension: String,
    pub classifier: String,
}

impl Key {
    pub fn new(group: &str, artifact: &str) -> Self {
        Self {
            group: group.to_owned(),
            artifact: artifact.to_owned(),
            extension: "jar".to_owned(),
            classifier: String::new(),
        }
    }

    /// The key for a dependency of `type`, mapped to a file extension and
    /// classifier as Maven's default artifact handlers do. `bundle`, which
    /// only the Felix plugin's handler knows, is a jar.
    pub fn of_type(group: &str, artifact: &str, typ: &str, classifier: &str) -> Self {
        let (extension, implied) = match typ {
            "" | "jar" | "bundle" | "maven-plugin" | "ejb" => ("jar", ""),
            "test-jar" => ("jar", "tests"),
            "ejb-client" => ("jar", "client"),
            "java-source" => ("jar", "sources"),
            "javadoc" => ("jar", "javadoc"),
            other => (other, ""),
        };
        Self {
            group: group.to_owned(),
            artifact: artifact.to_owned(),
            extension: extension.to_owned(),
            classifier: if classifier.is_empty() { implied } else { classifier }.to_owned(),
        }
    }

    pub fn is_pom(&self) -> bool {
        self.extension == "pom"
    }
}

/// `group:artifact`, with the extension and classifier only when they
/// differ from a plain jar.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)?;
        match (self.extension.as_str(), self.classifier.as_str()) {
            ("jar", "") => Ok(()),
            (extension, "") => write!(f, ":{extension}"),
            (extension, classifier) => write!(f, ":{extension}:{classifier}"),
        }
    }
}

impl FromStr for Key {
    type Err = BoxError;

    fn from_str(s: &str) -> Result<Self, BoxError> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(format!("invalid Maven key `{s}`").into());
        }
        match parts.as_slice() {
            [group, artifact] => Ok(Self::new(group, artifact)),
            [group, artifact, extension] => Ok(Self {
                extension: (*extension).to_owned(),
                ..Self::new(group, artifact)
            }),
            [group, artifact, extension, classifier] => Ok(Self {
                extension: (*extension).to_owned(),
                classifier: (*classifier).to_owned(),
                ..Self::new(group, artifact)
            }),
            _ => Err(format!("invalid Maven key `{s}`: expected `group:artifact[:extension[:classifier]]`").into()),
        }
    }
}

impl serde::Serialize for Key {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Key {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(|e: BoxError| serde::de::Error::custom(e.to_string()))
    }
}

/// One version of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::providers::java) struct Artifact {
    pub key: Key,
    pub version: String,
}

impl Artifact {
    /// The artifact's path in a Maven repository.
    pub fn path(&self) -> String {
        let key = &self.key;
        let classifier = if key.classifier.is_empty() {
            String::new()
        } else {
            format!("-{}", key.classifier)
        };
        format!(
            "{}/{}/{}/{}-{}{classifier}.{}",
            key.group.replace('.', "/"),
            key.artifact,
            self.version,
            key.artifact,
            self.version,
            key.extension
        )
    }

    /// The POM describing this artifact.
    pub fn pom(&self) -> Artifact {
        Artifact {
            key: Key {
                extension: "pom".to_owned(),
                classifier: String::new(),
                ..self.key.clone()
            },
            version: self.version.clone(),
        }
    }
}

impl fmt::Display for Artifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.key, self.version)
    }
}

/// An artifact a user asks for, in the coordinate forms Maven's tools
/// accept: `group:artifact[:extension[:classifier]]:version`. The version
/// may be left out, as `group:artifact`, when a BOM provides it.
pub(in crate::providers::java) fn parse_request(s: &str) -> Result<(Key, Option<String>), BoxError> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("invalid Maven coordinate `{s}`").into());
    }
    let key = |group: &str, artifact: &str, extension: &str, classifier: &str| Key {
        extension: extension.to_owned(),
        classifier: classifier.to_owned(),
        ..Key::new(group, artifact)
    };
    match parts.as_slice() {
        [group, artifact] => Ok((Key::new(group, artifact), None)),
        [group, artifact, version] => Ok((Key::new(group, artifact), Some((*version).to_owned()))),
        [group, artifact, extension, version] => Ok((key(group, artifact, extension, ""), Some((*version).to_owned()))),
        [group, artifact, extension, classifier, version] => {
            Ok((key(group, artifact, extension, classifier), Some((*version).to_owned())))
        }
        _ => Err(
            format!("invalid Maven coordinate `{s}`: expected `group:artifact[:extension[:classifier]]:version`")
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_their_short_form() {
        for s in [
            "com.google.guava:guava",
            "org.example:bom:pom",
            "org.example:lib:jar:tests",
        ] {
            assert_eq!(s.parse::<Key>().unwrap().to_string(), s);
        }
        assert!("com.example".parse::<Key>().is_err());
        assert!("a:b:c:d:e".parse::<Key>().is_err());
    }

    #[test]
    fn types_map_to_extensions_and_classifiers() {
        assert_eq!(Key::of_type("g", "a", "test-jar", "").to_string(), "g:a:jar:tests");
        assert_eq!(Key::of_type("g", "a", "bundle", "").to_string(), "g:a");
        assert_eq!(Key::of_type("g", "a", "pom", "").to_string(), "g:a:pom");
        assert_eq!(
            Key::of_type("g", "a", "jar", "linux-x86_64").to_string(),
            "g:a:jar:linux-x86_64"
        );
    }

    #[test]
    fn artifacts_map_to_repository_paths() {
        let artifact = Artifact {
            key: Key::new("com.google.guava", "guava"),
            version: "33.4.0-jre".into(),
        };
        assert_eq!(
            artifact.path(),
            "com/google/guava/guava/33.4.0-jre/guava-33.4.0-jre.jar"
        );
        assert_eq!(
            artifact.pom().path(),
            "com/google/guava/guava/33.4.0-jre/guava-33.4.0-jre.pom"
        );
        let native = Artifact {
            key: Key::of_type("io.netty", "netty-transport-native-epoll", "jar", "linux-x86_64"),
            version: "4.1.0".into(),
        };
        assert!(
            native
                .path()
                .ends_with("/netty-transport-native-epoll-4.1.0-linux-x86_64.jar")
        );
    }

    #[test]
    fn requests_accept_maven_coordinate_forms() {
        assert_eq!(
            parse_request("g:a:1.0").unwrap(),
            (Key::new("g", "a"), Some("1.0".into()))
        );
        assert_eq!(parse_request("g:a").unwrap(), (Key::new("g", "a"), None));
        assert_eq!(
            parse_request("g:a:jar:tests:1.0").unwrap().0.to_string(),
            "g:a:jar:tests"
        );
        assert!(parse_request("g::1.0").is_err());
        assert!(parse_request("g").is_err());
    }
}
