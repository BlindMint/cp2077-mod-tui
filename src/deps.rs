use std::collections::{BTreeMap, BTreeSet};

use crate::models::{Dependency, LoadoutEntry, ResolvedEntry};

pub fn resolve(entries: &[LoadoutEntry], deps: &[Dependency]) -> Vec<ResolvedEntry> {
    let requested: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| entry.requested_enabled)
        .map(|entry| entry.mod_id.as_str())
        .collect();
    let graph: BTreeMap<&str, Vec<&str>> = deps.iter().fold(BTreeMap::new(), |mut map, dep| {
        map.entry(dep.mod_id.as_str())
            .or_default()
            .push(dep.requires_id.as_str());
        map
    });
    let mut memo = BTreeMap::new();
    entries
        .iter()
        .map(|entry| {
            if !entry.requested_enabled {
                return ResolvedEntry {
                    entry: entry.clone(),
                    effective_enabled: false,
                    disabled_reason: Some("disabled by user".into()),
                };
            }
            let mut visiting = BTreeSet::new();
            let result = check(
                entry.mod_id.as_str(),
                &requested,
                &graph,
                &mut memo,
                &mut visiting,
            );
            ResolvedEntry {
                entry: entry.clone(),
                effective_enabled: result.is_ok(),
                disabled_reason: result.err(),
            }
        })
        .collect()
}

fn check<'a>(
    id: &'a str,
    requested: &BTreeSet<&'a str>,
    graph: &BTreeMap<&'a str, Vec<&'a str>>,
    memo: &mut BTreeMap<&'a str, Result<(), String>>,
    visiting: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if let Some(result) = memo.get(id) {
        return result.clone();
    }
    if !visiting.insert(id) {
        return Err(format!("dependency cycle involving {id}"));
    }
    for requirement in graph.get(id).into_iter().flatten() {
        if !requested.contains(requirement) {
            let error = format!("requires disabled or missing {requirement}");
            memo.insert(id, Err(error.clone()));
            visiting.remove(id);
            return Err(error);
        }
        check(requirement, requested, graph, memo, visiting)
            .map_err(|error| format!("requires {requirement}: {error}"))?;
    }
    visiting.remove(id);
    memo.insert(id, Ok(()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, enabled: bool) -> LoadoutEntry {
        LoadoutEntry {
            profile_id: "p".into(),
            mod_id: id.into(),
            priority: 0,
            requested_enabled: enabled,
        }
    }

    #[test]
    fn cascade_disables_but_preserves_request() {
        let entries = vec![entry("red4ext", false), entry("archivexl", true)];
        let deps = vec![Dependency {
            mod_id: "archivexl".into(),
            requires_id: "red4ext".into(),
            inferred: false,
        }];
        let result = resolve(&entries, &deps);
        assert!(!result[1].effective_enabled);
        assert!(result[1].entry.requested_enabled);
        assert!(
            result[1]
                .disabled_reason
                .as_deref()
                .unwrap()
                .contains("red4ext")
        );
    }

    #[test]
    fn reports_cycles() {
        let entries = vec![entry("a", true), entry("b", true)];
        let deps = vec![
            Dependency {
                mod_id: "a".into(),
                requires_id: "b".into(),
                inferred: false,
            },
            Dependency {
                mod_id: "b".into(),
                requires_id: "a".into(),
                inferred: false,
            },
        ];
        assert!(
            resolve(&entries, &deps)
                .iter()
                .all(|entry| !entry.effective_enabled)
        );
    }
}
