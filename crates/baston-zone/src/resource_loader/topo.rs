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

/// A start order, and the resources that cannot have one.
///
/// Separating them is the point: a manifest naming a dependency nobody
/// installed, or two resources waiting on each other, used to fail the whole
/// startup. Only the resources actually involved are unstartable — the rest of
/// the server has no reason to care.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartPlan {
    /// Startable resources, dependencies first.
    pub order: Vec<String>,
    /// `(resource, why)` for those that can never start as configured.
    pub unsatisfiable: Vec<(String, String)>,
}

/// Order what can be ordered, and say what cannot.
///
/// Unlike [`topo_sort`], this never fails. A resource whose dependency is
/// missing is dropped along with everything that waits on it, and whatever
/// remains in a cycle is dropped too; both are reported rather than raised.
pub fn plan(resources: &HashMap<String, Vec<String>>) -> StartPlan {
    let mut unsatisfiable: Vec<(String, String)> = Vec::new();
    let mut remaining: HashMap<String, Vec<String>> = resources.clone();

    // Drop resources with a dependency nobody installed, then whatever was
    // waiting on them, until the set stops shrinking.
    loop {
        let doomed: Vec<(String, String)> = remaining
            .iter()
            .filter_map(|(name, deps)| {
                let missing = deps.iter().find(|d| !remaining.contains_key(*d))?;
                Some((name.clone(), missing.clone()))
            })
            .collect();
        if doomed.is_empty() {
            break;
        }
        for (name, missing) in doomed {
            remaining.remove(&name);
            unsatisfiable.push((
                name,
                format!("depends on {missing:?}, which is not installed"),
            ));
        }
    }

    match topo_sort(&remaining) {
        Ok(order) => {
            unsatisfiable.sort_unstable();
            StartPlan {
                order,
                unsatisfiable,
            }
        }
        Err(ZoneError::CyclicDependency(cycle)) => {
            // Only the cycle is unstartable; sorting without it succeeds.
            for name in &cycle {
                remaining.remove(name);
                unsatisfiable.push((
                    name.clone(),
                    format!("in a dependency cycle: {}", cycle.join(" -> ")),
                ));
            }
            unsatisfiable.sort_unstable();
            StartPlan {
                order: topo_sort(&remaining).unwrap_or_default(),
                unsatisfiable,
            }
        }
        // `remaining` has no unknown dependencies left, so no other error is
        // reachable; treating one as "nothing starts" still keeps us alive.
        Err(_) => StartPlan {
            order: Vec::new(),
            unsatisfiable,
        },
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(n, deps)| {
                (
                    (*n).to_owned(),
                    deps.iter().map(|d| (*d).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_healthy_graph_plans_dependencies_first() {
        let plan = plan(&graph(&[("app", &["lib"]), ("lib", &[])]));
        assert_eq!(plan.order, vec!["lib", "app"]);
        assert!(plan.unsatisfiable.is_empty());
    }

    /// The case that used to deny the whole server: one manifest naming a
    /// resource nobody installed.
    #[test]
    fn a_missing_dependency_costs_only_its_dependents() {
        let plan = plan(&graph(&[
            ("chat", &[]),
            ("map", &["money-fountain"]),
            ("spawn", &[]),
        ]));
        assert_eq!(plan.order, vec!["chat", "spawn"]);
        assert_eq!(plan.unsatisfiable.len(), 1);
        assert_eq!(plan.unsatisfiable[0].0, "map");
        assert!(plan.unsatisfiable[0].1.contains("money-fountain"));
    }

    #[test]
    fn the_loss_carries_to_whatever_waited_on_it() {
        let plan = plan(&graph(&[("a", &["ghost"]), ("b", &["a"]), ("c", &[])]));
        assert_eq!(plan.order, vec!["c"]);
        let names: Vec<&str> = plan.unsatisfiable.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn a_cycle_costs_only_the_cycle() {
        let plan = plan(&graph(&[("a", &["b"]), ("b", &["a"]), ("alone", &[])]));
        assert_eq!(plan.order, vec!["alone"]);
        assert_eq!(plan.unsatisfiable.len(), 2);
        assert!(plan.unsatisfiable[0].1.contains("cycle"));
    }

    #[test]
    fn nothing_installed_is_not_a_failure() {
        assert_eq!(plan(&HashMap::new()), StartPlan::default());
    }
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
