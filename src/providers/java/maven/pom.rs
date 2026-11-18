//! A POM file as written, before inheritance, interpolation or profile
//! activation. Only what dependency resolution reads is kept.

use roxmltree::Node;

use crate::provider::BoxError;

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Pom {
    pub parent: Option<Parent>,
    pub group: Option<String>,
    pub artifact: Option<String>,
    pub version: Option<String>,
    pub packaging: Option<String>,
    pub properties: Vec<(String, String)>,
    pub dependencies: Vec<Dependency>,
    pub management: Vec<Dependency>,
    pub profiles: Vec<Profile>,
    pub relocation: Option<Relocation>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Parent {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

/// A dependency as declared. Every field may still hold `${…}`
/// expressions.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Dependency {
    pub group: String,
    pub artifact: String,
    pub version: Option<String>,
    pub typ: Option<String>,
    pub classifier: Option<String>,
    pub scope: Option<String>,
    pub optional: Option<String>,
    pub exclusions: Vec<Exclusion>,
}

/// An excluded `group:artifact`, where either may be the wildcard `*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Exclusion {
    pub group: String,
    pub artifact: String,
}

impl Exclusion {
    pub fn matches(&self, group: &str, artifact: &str) -> bool {
        (self.group == "*" || self.group == group) && (self.artifact == "*" || self.artifact == artifact)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Profile {
    pub activation: Activation,
    pub properties: Vec<(String, String)>,
    pub dependencies: Vec<Dependency>,
    pub management: Vec<Dependency>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Activation {
    pub by_default: bool,
    pub jdk: Option<String>,
    /// The property's name and expected value, each possibly negated
    /// with `!`.
    pub property: Option<(String, Option<String>)>,
    /// An `os`, `file` or `packaging` condition, which bit does not
    /// evaluate.
    pub other: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Relocation {
    pub group: Option<String>,
    pub artifact: Option<String>,
    pub version: Option<String>,
}

pub(super) fn parse(xml: &str) -> Result<Pom, BoxError> {
    let document = roxmltree::Document::parse(xml)?;
    let project = document.root_element();
    if project.tag_name().name() != "project" {
        return Err("not a POM: the root element is not <project>".into());
    }
    Ok(Pom {
        parent: child(project, "parent")
            .map(|parent| -> Result<Parent, BoxError> {
                Ok(Parent {
                    group: required(parent, "groupId")?,
                    artifact: required(parent, "artifactId")?,
                    version: required(parent, "version")?,
                })
            })
            .transpose()?,
        group: text(project, "groupId"),
        artifact: text(project, "artifactId"),
        version: text(project, "version"),
        packaging: text(project, "packaging"),
        properties: properties(project),
        dependencies: dependencies(child(project, "dependencies"))?,
        management: dependencies(child(project, "dependencyManagement").and_then(|m| child(m, "dependencies")))?,
        profiles: children(child(project, "profiles"), "profile")
            .map(profile)
            .collect::<Result<_, _>>()?,
        relocation: child(project, "distributionManagement")
            .and_then(|management| child(management, "relocation"))
            .map(|relocation| Relocation {
                group: text(relocation, "groupId"),
                artifact: text(relocation, "artifactId"),
                version: text(relocation, "version"),
            }),
    })
}

fn profile(node: Node) -> Result<Profile, BoxError> {
    let activation = child(node, "activation").map(|activation| Activation {
        by_default: text(activation, "activeByDefault").as_deref() == Some("true"),
        jdk: text(activation, "jdk"),
        property: child(activation, "property")
            .and_then(|property| Some((text(property, "name")?, text(property, "value")))),
        other: ["os", "file", "packaging"]
            .iter()
            .any(|name| child(activation, name).is_some()),
    });
    Ok(Profile {
        activation: activation.unwrap_or_default(),
        properties: properties(node),
        dependencies: dependencies(child(node, "dependencies"))?,
        management: dependencies(child(node, "dependencyManagement").and_then(|m| child(m, "dependencies")))?,
    })
}

fn properties(node: Node) -> Vec<(String, String)> {
    children(child(node, "properties"), "")
        .map(|property| {
            let value = property.text().unwrap_or("").trim().to_owned();
            (property.tag_name().name().to_owned(), value)
        })
        .collect()
}

fn dependencies(node: Option<Node>) -> Result<Vec<Dependency>, BoxError> {
    children(node, "dependency")
        .map(|dependency| {
            Ok(Dependency {
                group: required(dependency, "groupId")?,
                artifact: required(dependency, "artifactId")?,
                version: text(dependency, "version"),
                typ: text(dependency, "type"),
                classifier: text(dependency, "classifier"),
                scope: text(dependency, "scope"),
                optional: text(dependency, "optional"),
                exclusions: children(child(dependency, "exclusions"), "exclusion")
                    .map(|exclusion| Exclusion {
                        group: text(exclusion, "groupId").unwrap_or_else(|| "*".to_owned()),
                        artifact: text(exclusion, "artifactId").unwrap_or_else(|| "*".to_owned()),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// The first child element named `name`. POMs declare a default namespace,
/// so names are compared without it.
fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

/// The child elements of `node` named `name`, or all of them when `name`
/// is empty.
fn children<'a, 'i: 'a>(node: Option<Node<'a, 'i>>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    node.into_iter().flat_map(move |node| {
        node.children()
            .filter(move |child| child.is_element() && (name.is_empty() || child.tag_name().name() == name))
    })
}

fn text(node: Node, name: &str) -> Option<String> {
    child(node, name).map(|child| child.text().unwrap_or("").trim().to_owned())
}

fn required(node: Node, name: &str) -> Result<String, BoxError> {
    text(node, name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("<{}> has no <{name}>", node.tag_name().name()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>parent</artifactId>
    <version>1.0</version>
  </parent>
  <artifactId>lib</artifactId>
  <properties>
    <guava.version>33.4.0-jre</guava.version>
  </properties>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.example</groupId>
        <artifactId>bom</artifactId>
        <version>2.0</version>
        <type>pom</type>
        <scope>import</scope>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>${guava.version}</version>
      <exclusions>
        <exclusion><groupId>com.google.code.findbugs</groupId><artifactId>*</artifactId></exclusion>
      </exclusions>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <scope>test</scope>
      <optional>true</optional>
    </dependency>
  </dependencies>
  <profiles>
    <profile>
      <id>jdk9</id>
      <activation><jdk>[9,)</jdk></activation>
      <dependencies>
        <dependency><groupId>javax.annotation</groupId><artifactId>api</artifactId><version>1</version></dependency>
      </dependencies>
    </profile>
    <profile>
      <id>linux</id>
      <activation><os><family>unix</family></os><property><name>!skip</name></property></activation>
    </profile>
  </profiles>
  <distributionManagement>
    <relocation><groupId>com.example.new</groupId></relocation>
  </distributionManagement>
</project>
"#;

    #[test]
    fn parses_what_resolution_reads() {
        let pom = parse(POM).unwrap();
        assert_eq!(
            pom.parent,
            Some(Parent {
                group: "com.example".into(),
                artifact: "parent".into(),
                version: "1.0".into()
            })
        );
        assert_eq!(pom.group, None);
        assert_eq!(pom.artifact.as_deref(), Some("lib"));
        assert_eq!(pom.properties, [("guava.version".to_owned(), "33.4.0-jre".to_owned())]);
        assert_eq!(pom.management[0].scope.as_deref(), Some("import"));
        assert_eq!(pom.dependencies[0].version.as_deref(), Some("${guava.version}"));
        assert_eq!(
            pom.dependencies[0].exclusions,
            [Exclusion {
                group: "com.google.code.findbugs".into(),
                artifact: "*".into()
            }]
        );
        assert_eq!(pom.dependencies[1].optional.as_deref(), Some("true"));
        assert_eq!(pom.profiles[0].activation.jdk.as_deref(), Some("[9,)"));
        assert_eq!(pom.profiles[0].dependencies[0].artifact, "api");
        assert!(pom.profiles[1].activation.other);
        assert_eq!(pom.profiles[1].activation.property, Some(("!skip".to_owned(), None)));
        assert_eq!(pom.relocation.unwrap().group.as_deref(), Some("com.example.new"));
    }

    #[test]
    fn exclusions_match_wildcards() {
        let exclusion = |group: &str, artifact: &str| Exclusion {
            group: group.into(),
            artifact: artifact.into(),
        };
        assert!(exclusion("*", "*").matches("a", "b"));
        assert!(exclusion("a", "*").matches("a", "b"));
        assert!(!exclusion("a", "c").matches("a", "b"));
        assert!(!exclusion("a*", "b").matches("ab", "b"));
    }

    #[test]
    fn rejects_other_documents() {
        assert!(parse("<settings/>").is_err());
        assert!(
            parse(
                "<project><dependencies><dependency><artifactId>a</artifactId></dependency></dependencies></project>"
            )
            .is_err()
        );
    }
}
