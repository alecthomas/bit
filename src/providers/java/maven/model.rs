//! Effective POMs: a POM with its active profiles, its parents and its
//! imported BOMs applied, and its expressions interpolated, following
//! Maven 3's model builder in that order.
//!
//! Profiles activate only on `activeByDefault`, `jdk`, and `property`
//! conditions. bit sets no properties, so a property condition holds only
//! when it asks for the property to be unset. `os`, `file` and `packaging`
//! conditions are treated as unmet: evaluating them would make the
//! lockfile depend on the machine that resolved it.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::provider::BoxError;

use super::coords::{Artifact, Key};
use super::pom::{self, Dependency, Exclusion, Pom, Profile};
use super::repo::Repositories;

/// Parent POMs followed before the chain is treated as a cycle.
const MAX_PARENTS: usize = 64;

/// Passes over a value while its properties expand to further properties.
const MAX_INTERPOLATION_PASSES: usize = 16;

/// A dependency of an effective POM, interpolated, with management applied.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Dep {
    pub key: Key,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub optional: Option<bool>,
    pub exclusions: Vec<Exclusion>,
}

impl Dep {
    pub fn scope(&self) -> &str {
        self.scope.as_deref().unwrap_or("compile")
    }
}

/// What resolution needs of an effective POM.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Model {
    pub dependencies: Vec<Dep>,
    pub management: Vec<Dep>,
    /// Where the artifact moved to, when its POM says so.
    pub relocation: Option<(String, String, String)>,
}

/// Group, artifact and version: what names a POM.
type Gav = (String, String, String);

/// Builds effective POMs, fetching each POM once.
pub(super) struct Models<'r> {
    repos: &'r Repositories,
    /// The JDK version `jdk` activations compare against.
    jdk: String,
    raw: HashMap<Gav, Rc<Pom>>,
    built: HashMap<Gav, Rc<Model>>,
    building: HashSet<Gav>,
}

impl<'r> Models<'r> {
    pub fn new(repos: &'r Repositories, jdk: String) -> Self {
        Self {
            repos,
            jdk,
            raw: HashMap::new(),
            built: HashMap::new(),
            building: HashSet::new(),
        }
    }

    fn raw(&mut self, gav: &Gav) -> Result<Rc<Pom>, BoxError> {
        if let Some(pom) = self.raw.get(gav) {
            return Ok(pom.clone());
        }
        let artifact = Artifact {
            key: Key::new(&gav.0, &gav.1),
            version: gav.2.clone(),
        }
        .pom();
        let path = self.repos.fetch(&artifact.path())?;
        let pom = pom::parse(&std::fs::read_to_string(&path)?).map_err(|e| format!("{artifact}: {e}"))?;
        let pom = Rc::new(pom);
        self.raw.insert(gav.clone(), pom.clone());
        Ok(pom)
    }

    /// The effective POM of `group:artifact:version`.
    pub fn model(&mut self, group: &str, artifact: &str, version: &str) -> Result<Rc<Model>, BoxError> {
        let gav = (group.to_owned(), artifact.to_owned(), version.to_owned());
        if let Some(model) = self.built.get(&gav) {
            return Ok(model.clone());
        }
        if !self.building.insert(gav.clone()) {
            return Err(format!("{group}:{artifact}:{version} imports itself").into());
        }
        let model = self.build(&gav);
        self.building.remove(&gav);
        let model = Rc::new(model.map_err(|e| format!("{group}:{artifact}:{version}: {e}"))?);
        self.built.insert(gav, model.clone());
        Ok(model)
    }

    fn build(&mut self, gav: &Gav) -> Result<Model, BoxError> {
        let merged = self.inherit(gav)?;
        let properties = properties(&merged, gav);
        let resolve = |value: &str| interpolate(value, &properties);
        let interpolated = |deps: &[Dependency]| -> Vec<Dependency> {
            deps.iter()
                .map(|dep| Dependency {
                    group: resolve(&dep.group),
                    artifact: resolve(&dep.artifact),
                    version: dep.version.as_deref().map(resolve),
                    typ: dep.typ.as_deref().map(resolve),
                    classifier: dep.classifier.as_deref().map(resolve),
                    scope: dep.scope.as_deref().map(resolve),
                    optional: dep.optional.as_deref().map(resolve),
                    exclusions: dep
                        .exclusions
                        .iter()
                        .map(|exclusion| Exclusion {
                            group: resolve(&exclusion.group),
                            artifact: resolve(&exclusion.artifact),
                        })
                        .collect(),
                })
                .collect()
        };

        // Imported BOMs add only what the POM and its parents don't manage,
        // the first import winning between imports.
        let mut management = Vec::new();
        let mut imports = Vec::new();
        for dep in interpolated(&merged.management) {
            if dep.scope.as_deref() == Some("import") && dep.typ.as_deref() == Some("pom") {
                imports.push(dep);
            } else {
                management.push(dep_of(&dep));
            }
        }
        for import in imports {
            let version = import
                .version
                .as_deref()
                .ok_or_else(|| format!("imported {}:{} has no version", import.group, import.artifact))?;
            let bom = self.model(&import.group, &import.artifact, version)?;
            for managed in &bom.management {
                if !management.iter().any(|existing: &Dep| existing.key == managed.key) {
                    management.push(managed.clone());
                }
            }
        }

        let dependencies = interpolated(&merged.dependencies)
            .iter()
            .map(|dep| inject(dep_of(dep), &management))
            .collect();
        let relocation = merged.relocation.as_ref().map(|relocation| {
            (
                relocation
                    .group
                    .as_deref()
                    .map(resolve)
                    .unwrap_or_else(|| gav.0.clone()),
                relocation
                    .artifact
                    .as_deref()
                    .map(resolve)
                    .unwrap_or_else(|| gav.1.clone()),
                relocation
                    .version
                    .as_deref()
                    .map(resolve)
                    .unwrap_or_else(|| gav.2.clone()),
            )
        });
        Ok(Model {
            dependencies,
            management,
            relocation,
        })
    }

    /// The POM merged with its parents, each with its active profiles
    /// applied first, as Maven's inheritance assembly does.
    fn inherit(&mut self, gav: &Gav) -> Result<Pom, BoxError> {
        let pom = self.raw(gav)?;
        let mut lineage = vec![self.activated(&pom)];
        while let Some(parent) = lineage.last().and_then(|pom| pom.parent.clone()) {
            if lineage.len() > MAX_PARENTS {
                return Err("its parent POMs form a cycle".into());
            }
            let parent = self.raw(&(parent.group, parent.artifact, parent.version))?;
            lineage.push(self.activated(&parent));
        }
        let mut merged = lineage.pop().unwrap_or_default();
        while let Some(child) = lineage.pop() {
            merged = merge(child, merged);
        }
        Ok(merged)
    }

    /// `pom` with the properties and dependencies of its active profiles
    /// added.
    fn activated(&self, pom: &Pom) -> Pom {
        let mut pom = pom.clone();
        let profiles = std::mem::take(&mut pom.profiles);
        let mut active: Vec<&Profile> = profiles.iter().filter(|profile| self.is_active(profile)).collect();
        if active.is_empty() {
            active = profiles
                .iter()
                .filter(|profile| profile.activation.by_default)
                .collect();
        }
        for profile in active {
            pom.properties.extend(profile.properties.iter().cloned());
            pom.dependencies.extend(profile.dependencies.iter().cloned());
            pom.management.extend(profile.management.iter().cloned());
        }
        pom
    }

    /// Whether every condition of the profile's activation holds. One
    /// without conditions only activates by default.
    fn is_active(&self, profile: &Profile) -> bool {
        let activation = &profile.activation;
        if activation.other || (activation.jdk.is_none() && activation.property.is_none()) {
            return false;
        }
        let jdk = activation
            .jdk
            .as_deref()
            .is_none_or(|spec| jdk_matches(spec, &self.jdk));
        let property = activation.property.as_ref().is_none_or(|(name, value)| {
            // No property is set, as with Maven run without `-D`.
            match value.as_deref() {
                Some(value) if !value.is_empty() => value.starts_with('!'),
                _ => name.starts_with('!'),
            }
        });
        jdk && property
    }
}

/// `child` inheriting from `parent`: the child's values win, and the
/// parent's dependencies and managed dependencies follow the child's
/// unless the child declares the same one.
fn merge(child: Pom, parent: Pom) -> Pom {
    let mut properties = parent.properties;
    properties.extend(child.properties);
    let combine = |own: Vec<Dependency>, inherited: Vec<Dependency>| {
        let mut deps = own;
        for dep in inherited {
            if !deps
                .iter()
                .any(|existing| management_key(existing) == management_key(&dep))
            {
                deps.push(dep);
            }
        }
        deps
    };
    Pom {
        group: child
            .group
            .or_else(|| child.parent.as_ref().map(|parent| parent.group.clone())),
        version: child
            .version
            .or_else(|| child.parent.as_ref().map(|parent| parent.version.clone())),
        parent: child.parent,
        artifact: child.artifact,
        packaging: child.packaging,
        properties,
        dependencies: combine(child.dependencies, parent.dependencies),
        management: combine(child.management, parent.management),
        profiles: Vec::new(),
        relocation: child.relocation,
    }
}

/// How Maven tells declarations of one dependency apart.
fn management_key(dep: &Dependency) -> (String, String, String, String) {
    (
        dep.group.clone(),
        dep.artifact.clone(),
        dep.typ.clone().unwrap_or_else(|| "jar".to_owned()),
        dep.classifier.clone().unwrap_or_default(),
    )
}

fn dep_of(dep: &Dependency) -> Dep {
    Dep {
        key: Key::of_type(
            &dep.group,
            &dep.artifact,
            dep.typ.as_deref().unwrap_or("jar"),
            dep.classifier.as_deref().unwrap_or(""),
        ),
        version: dep.version.clone(),
        scope: dep.scope.clone(),
        optional: dep.optional.as_deref().map(|optional| optional == "true"),
        exclusions: dep.exclusions.clone(),
    }
}

/// `dep` with what `management` says about it filling what it leaves
/// unset, and the managed exclusions added, as Maven's management
/// injector does for a POM's own dependencies.
fn inject(mut dep: Dep, management: &[Dep]) -> Dep {
    if let Some(managed) = management.iter().find(|managed| managed.key == dep.key) {
        dep.version = dep.version.or_else(|| managed.version.clone());
        dep.scope = dep.scope.or_else(|| managed.scope.clone());
        dep.optional = dep.optional.or(managed.optional);
        for exclusion in &managed.exclusions {
            if !dep.exclusions.contains(exclusion) {
                dep.exclusions.push(exclusion.clone());
            }
        }
    }
    dep
}

/// The values `${…}` can name: the POM's properties, then its coordinates
/// under `project.`, `pom.` and no prefix, Maven's legacy forms.
fn properties(pom: &Pom, gav: &Gav) -> HashMap<String, String> {
    let mut values: HashMap<String, String> = pom.properties.iter().cloned().collect();
    let parent = pom.parent.as_ref();
    let builtins = [
        ("groupId", Some(gav.0.clone())),
        ("artifactId", Some(gav.1.clone())),
        ("version", Some(gav.2.clone())),
        (
            "packaging",
            Some(pom.packaging.clone().unwrap_or_else(|| "jar".to_owned())),
        ),
        ("parent.groupId", parent.map(|parent| parent.group.clone())),
        ("parent.artifactId", parent.map(|parent| parent.artifact.clone())),
        ("parent.version", parent.map(|parent| parent.version.clone())),
    ];
    for (name, value) in builtins {
        let Some(value) = value else { continue };
        for prefix in ["project.", "pom.", ""] {
            values.entry(format!("{prefix}{name}")).or_insert_with(|| value.clone());
        }
    }
    values
}

/// `value` with each `${name}` that `properties` defines replaced, until
/// nothing more expands. Undefined expressions are left as written.
fn interpolate(value: &str, properties: &HashMap<String, String>) -> String {
    let mut current = value.to_owned();
    for _ in 0..MAX_INTERPOLATION_PASSES {
        if !current.contains("${") {
            break;
        }
        let mut out = String::new();
        let mut rest = current.as_str();
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find('}') else {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let name = &rest[start + 2..start + end];
            match properties.get(name) {
                Some(replacement) => out.push_str(replacement),
                None => out.push_str(&rest[start..=start + end]),
            }
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        if out == current {
            break;
        }
        current = out;
    }
    current
}

/// Whether a `jdk` activation matches the JDK version `jdk`: a version
/// prefix, `!` negating it, or a range such as `[11,)` or `(,1.8]`.
fn jdk_matches(spec: &str, jdk: &str) -> bool {
    if let Some(negated) = spec.strip_prefix('!') {
        return !jdk_matches(negated, jdk);
    }
    if !spec.starts_with(['[', '(']) {
        return jdk.starts_with(spec);
    }
    // A union of ranges, such as `[1.8,9),[11,)`.
    let mut ranges = Vec::new();
    let mut current = String::new();
    for c in spec.chars() {
        current.push(c);
        if c == ']' || c == ')' {
            ranges.push(std::mem::take(&mut current).trim_start_matches(',').to_owned());
        }
    }
    ranges.iter().any(|range| in_range(range, jdk))
}

fn in_range(range: &str, version: &str) -> bool {
    let inclusive_low = range.starts_with('[');
    let inclusive_high = range.ends_with(']');
    let inner = &range[1..range.len().saturating_sub(1)];
    let (low, high) = match inner.split_once(',') {
        Some((low, high)) => (low.trim(), high.trim()),
        None => (inner.trim(), inner.trim()),
    };
    let version = segments(version);
    let above = low.is_empty() || {
        let order = version.cmp(&segments(low));
        order.is_gt() || (inclusive_low && order.is_eq())
    };
    let below = high.is_empty() || {
        let order = version.cmp(&segments(high));
        order.is_lt() || (inclusive_high && order.is_eq())
    };
    above && below
}

/// The numeric parts of a JDK version, with trailing zeros dropped so that
/// `11` and `11.0` compare equal, and `1.8` read as `8`.
fn segments(version: &str) -> Vec<u64> {
    let mut parts: Vec<u64> = version
        .split(['.', '-', '_', '+'])
        .map_while(|part| part.parse().ok())
        .collect();
    if parts.first() == Some(&1) && parts.len() > 1 {
        parts.remove(0);
    }
    while parts.last() == Some(&0) {
        parts.pop();
    }
    parts
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    /// A file repository holding POMs given as `group:artifact:version`
    /// and their XML.
    pub(in crate::providers::java::maven) fn repo_with(poms: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (gav, xml) in poms {
            let parts: Vec<&str> = gav.split(':').collect();
            let artifact = Artifact {
                key: Key::new(parts[0], parts[1]),
                version: parts[2].to_owned(),
            }
            .pom();
            let path = dir.path().join(artifact.path());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, xml).unwrap();
        }
        dir
    }

    /// Repositories reading only `remote`, caching into `cache`, and
    /// ignoring this machine's Maven settings.
    pub(in crate::providers::java::maven) fn repos(remote: &Path, cache: &Path) -> Repositories {
        let dirs = super::super::repo::MavenDirs {
            cache: cache.to_path_buf(),
            user: None,
        };
        Repositories::configured(&[format!("file://{}", remote.display())], &dirs).unwrap()
    }

    fn deps(model: &Model) -> Vec<String> {
        model
            .dependencies
            .iter()
            .map(|dep| format!("{}:{}:{}", dep.key, dep.version.as_deref().unwrap_or("?"), dep.scope()))
            .collect()
    }

    #[test]
    fn inherits_interpolates_and_injects_management() {
        let remote = repo_with(&[
            (
                "com.example:parent:1",
                r#"<project><groupId>com.example</groupId><artifactId>parent</artifactId><version>1</version>
                <properties><guava.version>33.0</guava.version><slf4j.version>1.0</slf4j.version></properties>
                <dependencyManagement><dependencies>
                  <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId><version>${slf4j.version}</version><scope>runtime</scope></dependency>
                  <dependency><groupId>com.example</groupId><artifactId>bom</artifactId><version>2</version><type>pom</type><scope>import</scope></dependency>
                </dependencies></dependencyManagement>
                <dependencies><dependency><groupId>com.example</groupId><artifactId>common</artifactId><version>${project.version}</version></dependency></dependencies>
                </project>"#,
            ),
            (
                "com.example:bom:2",
                r#"<project><groupId>com.example</groupId><artifactId>bom</artifactId><version>2</version>
                <dependencyManagement><dependencies>
                  <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId><version>9.9</version></dependency>
                  <dependency><groupId>com.fasterxml</groupId><artifactId>jackson</artifactId><version>2.18</version></dependency>
                </dependencies></dependencyManagement></project>"#,
            ),
            (
                "com.example:lib:3",
                r#"<project><parent><groupId>com.example</groupId><artifactId>parent</artifactId><version>1</version></parent>
                <artifactId>lib</artifactId><version>3</version>
                <properties><slf4j.version>2.0</slf4j.version></properties>
                <dependencies>
                  <dependency><groupId>com.google.guava</groupId><artifactId>guava</artifactId><version>${guava.version}</version></dependency>
                  <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId></dependency>
                  <dependency><groupId>com.fasterxml</groupId><artifactId>jackson</artifactId></dependency>
                </dependencies></project>"#,
            ),
        ]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repos(remote.path(), cache.path());
        let mut models = Models::new(&repos, "25.0.4".into());
        let model = models.model("com.example", "lib", "3").unwrap();
        assert_eq!(
            deps(&model),
            [
                "com.google.guava:guava:33.0:compile",
                // The child's property wins, and the parent's management
                // wins over the BOM it imports.
                "org.slf4j:slf4j-api:2.0:runtime",
                "com.fasterxml:jackson:2.18:compile",
                // `project.version` is the child's.
                "com.example:common:3:compile",
            ]
        );
    }

    #[test]
    fn applies_active_profiles() {
        let remote = repo_with(&[(
            "g:a:1",
            r#"<project><groupId>g</groupId><artifactId>a</artifactId><version>1</version>
            <profiles>
              <profile><activation><jdk>[17,)</jdk></activation><dependencies><dependency><groupId>g</groupId><artifactId>modern</artifactId><version>1</version></dependency></dependencies></profile>
              <profile><activation><jdk>1.8</jdk></activation><dependencies><dependency><groupId>g</groupId><artifactId>legacy</artifactId><version>1</version></dependency></dependencies></profile>
              <profile><activation><activeByDefault>true</activeByDefault></activation><dependencies><dependency><groupId>g</groupId><artifactId>default</artifactId><version>1</version></dependency></dependencies></profile>
              <profile><activation><property><name>!skipExtras</name></property><os><family>unix</family></os></activation><dependencies><dependency><groupId>g</groupId><artifactId>unix</artifactId><version>1</version></dependency></dependencies></profile>
            </profiles></project>"#,
        )]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repos(remote.path(), cache.path());
        let modern = Models::new(&repos, "25.0.4".into()).model("g", "a", "1").unwrap();
        assert_eq!(deps(&modern), ["g:modern:1:compile"]);
        let old = Models::new(&repos, "1.8.0_402".into()).model("g", "a", "1").unwrap();
        assert_eq!(deps(&old), ["g:legacy:1:compile"]);
        let other = Models::new(&repos, "11.0.2".into()).model("g", "a", "1").unwrap();
        assert_eq!(deps(&other), ["g:default:1:compile"]);
    }

    #[test]
    fn reads_relocations() {
        let remote = repo_with(&[(
            "old:a:1",
            r#"<project><groupId>old</groupId><artifactId>a</artifactId><version>1</version>
            <distributionManagement><relocation><groupId>new</groupId></relocation></distributionManagement></project>"#,
        )]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repos(remote.path(), cache.path());
        let model = Models::new(&repos, "25".into()).model("old", "a", "1").unwrap();
        assert_eq!(model.relocation, Some(("new".into(), "a".into(), "1".into())));
    }

    #[test]
    fn interpolation_expands_nested_properties_and_keeps_unknown_ones() {
        let properties: HashMap<String, String> = [("a", "${b}-x"), ("b", "1"), ("loop", "${loop}")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(interpolate("v${a}", &properties), "v1-x");
        assert_eq!(interpolate("${env.HOME}", &properties), "${env.HOME}");
        assert_eq!(interpolate("${loop}", &properties), "${loop}");
    }

    #[test]
    fn jdk_activation_matches_prefixes_and_ranges() {
        assert!(jdk_matches("25", "25.0.4"));
        assert!(jdk_matches("!1.8", "25.0.4"));
        assert!(!jdk_matches("1.8", "25.0.4"));
        assert!(jdk_matches("1.8", "1.8.0_402"));
        assert!(jdk_matches("[11,)", "25.0.4"));
        assert!(!jdk_matches("(,11)", "25.0.4"));
        assert!(jdk_matches("[1.8,9),[21,)", "21"));
        assert!(jdk_matches("[1.8,9)", "1.8.0_402"));
        assert!(jdk_matches("[11,11]", "11.0"));
    }
}
