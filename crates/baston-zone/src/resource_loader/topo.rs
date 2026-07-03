//! Kahn topological sort over the resource dependency graph.

use std::collections::{HashMap, VecDeque};

use crate::ZoneError;

/// Order resources so every dependency starts before its dependents.
/// `resources` maps name → dependency list. Unknown dependencies error out;
/// cycles are reported with the names involved.
pub fn topo_sort(resources: &HashMap<String, Vec<String>>) -> Result<Vec<String>, ZoneError> {
    for (name, deps) in resources {
        for dep in deps {
            if !resources.contains_key(dep) {
                return Err(ZoneError::UnknownDependency {
                    resource: name.clone(),
                    dependency: dep.clone(),
                });
            }
        }
    }

    // in_degree[X] = number of dependencies X waits on; dependents[D] = who waits on D.
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, deps) in resources {
        in_degree.entry(name).or_insert(0);
        for dep in deps {
            *in_degree.entry(name).or_insert(0) += 1;
            dependents.entry(dep).or_default().push(name);
        }
    }

    let mut ready: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&n, _)| n)
        .collect();
    // Deterministic start order regardless of HashMap iteration.
    ready.make_contiguous().sort_unstable();

    let mut ordered = Vec::with_capacity(resources.len());
    while let Some(name) = ready.pop_front() {
        ordered.push(name.to_owned());
        let mut newly_ready = Vec::new();
        for &dependent in dependents.get(name).map(Vec::as_slice).unwrap_or(&[]) {
            let deg = in_degree
                .get_mut(dependent)
                .expect("dependent present in in_degree by construction");
            *deg -= 1;
            if *deg == 0 {
                newly_ready.push(dependent);
            }
        }
        newly_ready.sort_unstable();
        ready.extend(newly_ready);
    }

    if ordered.len() != resources.len() {
        let mut cycle: Vec<String> = in_degree
            .into_iter()
            .filter(|(_, deg)| *deg > 0)
            .map(|(n, _)| n.to_owned())
            .collect();
        cycle.sort_unstable();
        return Err(ZoneError::CyclicDependency(cycle));
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(n, deps)| (n.to_string(), deps.iter().map(|d| d.to_string()).collect()))
            .collect()
    }

    #[test]
    fn orders_dependencies_first() {
        let g = graph(&[
            ("axiom-economy", &["axiom-core"]),
            ("axiom-core", &[]),
            ("axiom-jobs", &["axiom-economy"]),
        ]);
        let order = topo_sort(&g).unwrap();
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("axiom-core") < pos("axiom-economy"));
        assert!(pos("axiom-economy") < pos("axiom-jobs"));
    }

    #[test]
    fn detects_cycle() {
        let g = graph(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]);
        match topo_sort(&g) {
            Err(ZoneError::CyclicDependency(names)) => {
                assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected cycle error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_dependency() {
        let g = graph(&[("a", &["ghost"])]);
        assert!(matches!(
            topo_sort(&g),
            Err(ZoneError::UnknownDependency { .. })
        ));
    }
}
