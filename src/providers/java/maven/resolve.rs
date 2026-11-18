//! Dependency resolution with Maven 3's rules, as Maven Resolver 1.9
//! applies them:
//!
//! - The root's managed dependencies, from the requested BOMs, set the
//!   version, scope and optionality of transitive dependencies, and add
//!   exclusions to direct ones too (`ClassicDependencyManager`).
//! - `test` and `provided` dependencies of dependencies are left out
//!   (`ScopeDependencySelector`), as are optional ones
//!   (`OptionalDependencySelector`) and `system` ones.
//! - A dependency's exclusions apply to everything beneath it
//!   (`ExclusionDependencySelector`).
//! - Between versions of one artifact, the one nearest the root wins, and
//!   the first declared between equally near ones
//!   (`NearestVersionSelector`).
//!
//! Visiting the graph breadth first, in declaration order, meets every
//! artifact first at its nearest, first declared occurrence, so the first
//! version seen wins, and a losing occurrence's dependencies are never
//! visited, as Maven drops them.

use std::collections::VecDeque;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::provider::BoxError;

use super::coords::Key;
use super::model::{Dep, Models};
use super::pom::Exclusion;

/// Relocations followed before the chain is treated as a cycle.
const MAX_RELOCATIONS: usize = 8;

/// A resolved artifact.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Node {
    pub version: String,
    /// What it depends on after resolution, each by the key that won.
    pub dependencies: Vec<Key>,
}

/// One occurrence of a dependency waiting to be visited.
struct Item {
    dep: Dep,
    depth: usize,
    parent: Option<Key>,
    /// Exclusions of the dependencies above this one.
    excluded: Rc<Vec<Exclusion>>,
}

/// Resolve `requested`, each with its version or `None` for one a BOM in
/// `boms` manages, into every artifact the result needs, in the order
/// resolution met them.
pub(super) fn resolve(
    models: &mut Models,
    requested: &[(Key, Option<String>)],
    boms: &[(Key, String)],
) -> Result<IndexMap<Key, Node>, BoxError> {
    let mut management: Vec<Dep> = Vec::new();
    for (bom, version) in boms {
        for managed in &models.model(&bom.group, &bom.artifact, version)?.management {
            if !management.iter().any(|existing| existing.key == managed.key) {
                management.push(managed.clone());
            }
        }
    }

    let mut queue: VecDeque<Item> = requested
        .iter()
        .map(|(key, version)| Item {
            dep: Dep {
                key: key.clone(),
                version: version.clone(),
                scope: None,
                optional: None,
                exclusions: Vec::new(),
            },
            depth: 1,
            parent: None,
            excluded: Rc::new(Vec::new()),
        })
        .collect();
    let mut graph: IndexMap<Key, Node> = IndexMap::new();

    while let Some(item) = queue.pop_front() {
        let Item {
            mut dep,
            depth,
            parent,
            excluded,
        } = item;
        if let Some(managed) = management.iter().find(|managed| managed.key == dep.key) {
            // The root's own requests keep their versions; only one left
            // out takes the managed version, as the model builder fills it.
            if depth >= 2 || dep.version.is_none() {
                dep.version = managed.version.clone().or(dep.version);
            }
            if depth >= 2 {
                dep.scope = managed.scope.clone().or(dep.scope);
                dep.optional = managed.optional.or(dep.optional);
            }
            for exclusion in &managed.exclusions {
                if !dep.exclusions.contains(exclusion) {
                    dep.exclusions.push(exclusion.clone());
                }
            }
        }
        if (depth >= 2 && matches!(dep.scope(), "test" | "provided" | "system"))
            || (depth >= 2 && dep.optional == Some(true))
        {
            continue;
        }
        let via = parent
            .as_ref()
            .map(|parent| format!(" (a dependency of {parent})"))
            .unwrap_or_default();
        let version = dep
            .version
            .as_deref()
            .ok_or_else(|| format!("{}{via} has no version", dep.key))
            .and_then(|version| fixed_version(version).map_err(|e| format!("{}{via}: {e}", dep.key)))?;

        let (key, version) = if graph.contains_key(&dep.key) {
            (dep.key.clone(), version)
        } else {
            relocated(models, dep.key.clone(), version)?
        };
        if let Some(parent) = &parent
            && let Some(node) = graph.get_mut(parent)
            && !node.dependencies.contains(&key)
        {
            node.dependencies.push(key.clone());
        }
        if graph.contains_key(&key) {
            continue;
        }
        graph.insert(
            key.clone(),
            Node {
                version: version.clone(),
                dependencies: Vec::new(),
            },
        );

        let mut below: Vec<Exclusion> = excluded.as_ref().clone();
        below.extend(dep.exclusions.iter().cloned());
        let below = Rc::new(below);
        let model = models.model(&key.group, &key.artifact, &version)?;
        for child in &model.dependencies {
            if below
                .iter()
                .any(|exclusion| exclusion.matches(&child.key.group, &child.key.artifact))
            {
                continue;
            }
            queue.push_back(Item {
                dep: child.clone(),
                depth: depth + 1,
                parent: Some(key.clone()),
                excluded: below.clone(),
            });
        }
    }
    Ok(graph)
}

/// `key` at `version`, or where its POM says it moved to.
fn relocated(models: &mut Models, mut key: Key, mut version: String) -> Result<(Key, String), BoxError> {
    for _ in 0..MAX_RELOCATIONS {
        let model = models.model(&key.group, &key.artifact, &version)?;
        match &model.relocation {
            Some((group, artifact, to)) if (group, artifact, to) != (&key.group, &key.artifact, &version) => {
                key = Key {
                    group: group.clone(),
                    artifact: artifact.clone(),
                    ..key
                };
                version = to.clone();
            }
            _ => return Ok((key, version)),
        }
    }
    Err(format!("{key}:{version} is relocated too many times").into())
}

/// A version bit can fetch as written: `[1.0]` pins one, but other ranges
/// and snapshots need repository metadata bit doesn't read yet.
fn fixed_version(version: &str) -> Result<String, BoxError> {
    if version.contains("${") {
        return Err(format!("version `{version}` uses a property bit can't resolve").into());
    }
    if let Some(pinned) = version.strip_prefix('[').and_then(|rest| rest.strip_suffix(']'))
        && !pinned.contains(',')
    {
        return Ok(pinned.to_owned());
    }
    if version.starts_with(['[', '(']) || version.contains(',') {
        return Err(format!("version range `{version}` is not supported; request a version explicitly").into());
    }
    if version.ends_with("-SNAPSHOT") {
        return Err(format!("snapshot version `{version}` is not supported").into());
    }
    Ok(version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::{repo_with, repos};
    use super::*;

    fn pom(gav: &str, deps: &str) -> (String, String) {
        let parts: Vec<&str> = gav.split(':').collect();
        let xml = format!(
            "<project><groupId>{}</groupId><artifactId>{}</artifactId><version>{}</version>{deps}</project>",
            parts[0], parts[1], parts[2]
        );
        (gav.to_owned(), xml)
    }

    fn dep(gav: &str, extra: &str) -> String {
        let parts: Vec<&str> = gav.split(':').collect();
        let version = parts
            .get(2)
            .map(|v| format!("<version>{v}</version>"))
            .unwrap_or_default();
        format!(
            "<dependency><groupId>{}</groupId><artifactId>{}</artifactId>{version}{extra}</dependency>",
            parts[0], parts[1]
        )
    }

    fn deps(items: &[String]) -> String {
        format!("<dependencies>{}</dependencies>", items.concat())
    }

    /// Resolve `requested` against `poms` and render each artifact as
    /// `key:version -> dependencies`.
    fn resolved(poms: &[(String, String)], requested: &[&str], boms: &[&str]) -> Result<Vec<String>, BoxError> {
        let poms: Vec<(&str, &str)> = poms.iter().map(|(gav, xml)| (gav.as_str(), xml.as_str())).collect();
        let remote = repo_with(&poms);
        let cache = tempfile::tempdir().unwrap();
        let repos = repos(remote.path(), cache.path());
        let mut models = Models::new(&repos, "25".into());
        let requested: Vec<_> = requested
            .iter()
            .map(|request| super::super::coords::parse_request(request).unwrap())
            .collect();
        let boms: Vec<_> = boms
            .iter()
            .map(|bom| {
                let (key, version) = super::super::coords::parse_request(bom).unwrap();
                (key, version.unwrap())
            })
            .collect();
        let graph = resolve(&mut models, &requested, &boms)?;
        Ok(graph
            .iter()
            .map(|(key, node)| {
                let deps: Vec<String> = node.dependencies.iter().map(Key::to_string).collect();
                format!("{key}:{} -> [{}]", node.version, deps.join(", "))
            })
            .collect())
    }

    #[test]
    fn the_nearest_then_first_declared_version_wins() {
        let poms = [
            pom(
                "g:app:1",
                &deps(&[dep("g:a:1", ""), dep("g:b:1", ""), dep("g:c:1", "")]),
            ),
            pom("g:a:1", &deps(&[dep("g:shared:1", "")])),
            pom("g:b:1", &deps(&[dep("g:shared:2", ""), dep("g:only-b:1", "")])),
            pom("g:c:1", &deps(&[dep("g:deep:1", "")])),
            pom("g:deep:1", &deps(&[dep("g:a:2", "")])),
            pom("g:a:2", &deps(&[dep("g:never:1", "")])),
            pom("g:shared:1", ""),
            pom("g:shared:2", &deps(&[dep("g:dropped:1", "")])),
            pom("g:only-b:1", ""),
        ];
        assert_eq!(
            resolved(&poms, &["g:app:1"], &[]).unwrap(),
            [
                "g:app:1 -> [g:a, g:b, g:c]",
                "g:a:1 -> [g:shared]",
                "g:b:1 -> [g:shared, g:only-b]",
                "g:c:1 -> [g:deep]",
                "g:shared:1 -> []",
                "g:only-b:1 -> []",
                // The deeper g:a:2 loses, and its dependencies with it.
                "g:deep:1 -> [g:a]",
            ]
        );
    }

    #[test]
    fn transitive_test_provided_and_optional_dependencies_are_left_out() {
        let poms = [
            pom(
                "g:lib:1",
                &deps(&[
                    dep("g:junit:1", "<scope>test</scope>"),
                    dep("g:servlet:1", "<scope>provided</scope>"),
                    dep("g:extra:1", "<optional>true</optional>"),
                    dep("g:runtime:1", "<scope>runtime</scope>"),
                ]),
            ),
            pom("g:runtime:1", ""),
            pom("g:junit:1", ""),
        ];
        assert_eq!(
            resolved(&poms, &["g:lib:1", "g:junit:1"], &[]).unwrap(),
            ["g:lib:1 -> [g:runtime]", "g:junit:1 -> []", "g:runtime:1 -> []"]
        );
    }

    #[test]
    fn exclusions_apply_to_everything_beneath() {
        let poms = [
            pom(
                "g:app:1",
                &deps(&[dep(
                    "g:a:1",
                    "<exclusions><exclusion><groupId>g</groupId><artifactId>logging</artifactId></exclusion></exclusions>",
                )]),
            ),
            pom("g:a:1", &deps(&[dep("g:b:1", "")])),
            pom("g:b:1", &deps(&[dep("g:logging:1", ""), dep("g:kept:1", "")])),
            pom("g:kept:1", ""),
        ];
        assert_eq!(
            resolved(&poms, &["g:app:1"], &[]).unwrap(),
            [
                "g:app:1 -> [g:a]",
                "g:a:1 -> [g:b]",
                "g:b:1 -> [g:kept]",
                "g:kept:1 -> []"
            ]
        );
    }

    #[test]
    fn requested_boms_manage_transitive_versions_and_fill_missing_ones() {
        let poms = [
            pom(
                "g:bom:1",
                "<dependencyManagement><dependencies>\
                   <dependency><groupId>g</groupId><artifactId>shared</artifactId><version>9</version></dependency>\
                   <dependency><groupId>g</groupId><artifactId>direct</artifactId><version>5</version></dependency>\
                 </dependencies></dependencyManagement>",
            ),
            pom("g:a:1", &deps(&[dep("g:shared:1", "")])),
            pom("g:shared:9", ""),
            pom("g:direct:5", ""),
            pom("g:direct:4", ""),
        ];
        assert_eq!(
            resolved(&poms, &["g:a:1", "g:direct"], &["g:bom:1"]).unwrap(),
            ["g:a:1 -> [g:shared]", "g:direct:5 -> []", "g:shared:9 -> []"]
        );
        // An explicit version wins over the BOM's for a requested artifact.
        assert_eq!(
            resolved(&poms, &["g:direct:4"], &["g:bom:1"]).unwrap(),
            ["g:direct:4 -> []"]
        );
    }

    #[test]
    fn relocations_are_followed() {
        let poms = [
            pom("g:app:1", &deps(&[dep("old:lib:1", "")])),
            pom(
                "old:lib:1",
                "<distributionManagement><relocation><groupId>new</groupId><version>2</version></relocation></distributionManagement>",
            ),
            pom("new:lib:2", ""),
        ];
        assert_eq!(
            resolved(&poms, &["g:app:1"], &[]).unwrap(),
            ["g:app:1 -> [new:lib]", "new:lib:2 -> []"]
        );
    }

    #[test]
    fn unsupported_versions_name_the_dependency() {
        let poms = [pom("g:app:1", &deps(&[dep("g:a:[1.0,2.0)", "")]))];
        let error = resolved(&poms, &["g:app:1"], &[]).unwrap_err().to_string();
        assert!(error.contains("g:a (a dependency of g:app): version range"), "{error}");
        assert_eq!(fixed_version("[1.2]").unwrap(), "1.2");
        assert!(fixed_version("1.0-SNAPSHOT").is_err());
    }
}
