use std::collections::BTreeSet;

use crate::model::{DepError, Requirement};

/// Resolve the run order for `target` and its transitive `Requires:`: each
/// dependency comes before the job that needs it, `target` comes last, and every
/// job appears at most once (a diamond runs its shared dependency once). The
/// caller supplies `requires_of`, which returns a job's declared dependency names,
/// or `None` if the name is not a known job (so a typo in `Requires:` is a hard
/// error, not a silent skip). Pure: no filesystem or process access.
///
/// The traversal is iterative (an explicit work stack, not native recursion), so a
/// pathologically deep chain cannot overflow the call stack and abort the process.
pub(crate) fn dependency_order(
    target: &str,
    requires_of: impl Fn(&str) -> Option<Vec<Requirement>>,
) -> Result<Vec<Step>, DepError> {
    // Each frame is a job whose dependencies we are still walking (`next` is the
    // index of the next dependency to descend into). A post-order DFS: a frame
    // moves to `order` only once all its dependencies are done.
    struct Frame {
        name: String,
        args: Vec<String>,
        deps: Vec<Requirement>,
        next: usize,
    }

    let mut order: Vec<Step> = Vec::new();
    // Keyed on name *and* arguments: a diamond whose two paths ask for the same
    // thing still runs it once, but two callers asking for `(dist a)` and
    // `(dist b)` genuinely want two runs, and collapsing them would silently
    // drop one caller's request.
    let mut done: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
    // Cycle detection is on the name alone: `a` needing `(b x)` needing `(a y)`
    // is a cycle however the arguments differ, and keying this on arguments too
    // would let it spin forever generating new pairs.
    let mut on_stack = BTreeSet::new();
    let mut stack: Vec<Frame> = Vec::new();

    let deps = requires_of(target).ok_or_else(|| DepError::Missing {
        task: target.to_string(),
        required_by: target.to_string(),
    })?;
    on_stack.insert(target.to_string());
    stack.push(Frame {
        name: target.to_string(),
        args: Vec::new(),
        deps,
        next: 0,
    });

    loop {
        // Decide the next move using a short-lived borrow of the top frame, so the
        // stack is free to push/pop afterwards.
        let descend = {
            let Some(frame) = stack.last_mut() else { break };
            if frame.next < frame.deps.len() {
                let dep = frame.deps[frame.next].clone();
                frame.next += 1;
                Some(dep)
            } else {
                None
            }
        };
        match descend {
            Some(dep) => {
                let key = (dep.name.clone(), dep.args.clone());
                if done.contains(&key) {
                    continue; // already resolved via another path (a diamond)
                }
                if on_stack.contains(&dep.name) {
                    return Err(DepError::Cycle(dep.name));
                }
                let required_by = stack.last().expect("a top frame exists").name.clone();
                let deps = requires_of(&dep.name).ok_or(DepError::Missing {
                    task: dep.name.clone(),
                    required_by,
                })?;
                on_stack.insert(dep.name.clone());
                stack.push(Frame {
                    name: dep.name,
                    args: dep.args,
                    deps,
                    next: 0,
                });
            }
            None => {
                let frame = stack.pop().expect("a top frame exists");
                on_stack.remove(&frame.name);
                done.insert((frame.name.clone(), frame.args.clone()));
                order.push(Step {
                    name: frame.name,
                    args: frame.args,
                });
            }
        }
    }
    Ok(order)
}

/// One resolved step of a dependency chain: a job, and the arguments it runs
/// with. The target's own arguments come from the caller, so its `args` here is
/// empty and filled in by the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) name: String,
    pub(crate) args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A requirement with no arguments, which is what most of these graphs are.
    fn bare(name: &str) -> Requirement {
        Requirement {
            name: name.to_string(),
            args: Vec::new(),
        }
    }

    /// The names of a planned order. Ordering is what these tests are about, so
    /// they read better against names than against whole `Step`s.
    fn step_names(steps: &[Step]) -> Vec<&str> {
        steps.iter().map(|s| s.name.as_str()).collect()
    }

    // A `requires_of` for tests: a map from job name to its dependency names.
    fn deps_of<'a>(map: &'a [(&str, &[&str])]) -> impl Fn(&str) -> Option<Vec<Requirement>> + 'a {
        move |name| {
            map.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, ds)| ds.iter().map(|s| bare(s)).collect())
        }
    }

    #[test]
    fn dependency_order_is_deps_first_target_last() {
        // a -> b -> c, plus a -> c: c runs once, before b, and a is last.
        let g = deps_of(&[("a", &["b", "c"]), ("b", &["c"]), ("c", &[])]);
        assert_eq!(
            step_names(&dependency_order("a", g).unwrap()),
            ["c", "b", "a"]
        );
    }

    #[test]
    fn dependency_order_dedupes_a_diamond() {
        let g = deps_of(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])]);
        let planned = dependency_order("a", g).unwrap();
        let order = step_names(&planned);
        assert_eq!(order.iter().filter(|n| **n == "d").count(), 1);
        // d before b and c; a last.
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("d") < pos("b") && pos("d") < pos("c"));
        assert_eq!(*order.last().unwrap(), "a");
    }

    #[test]
    fn dependency_order_detects_a_cycle() {
        let g = deps_of(&[("a", &["b"]), ("b", &["a"])]);
        assert_eq!(dependency_order("a", g), Err(DepError::Cycle("a".into())));
    }

    #[test]
    fn dependency_order_flags_a_missing_dependency() {
        let g = deps_of(&[("a", &["ghost"])]);
        assert_eq!(
            dependency_order("a", g),
            Err(DepError::Missing {
                task: "ghost".into(),
                required_by: "a".into(),
            })
        );
    }

    #[test]
    fn dependency_order_survives_a_pathologically_deep_chain() {
        // t0 -> t1 -> ... -> tN. Native recursion overflowed the stack here; the
        // iterative walk must return a full, correctly ordered chain instead.
        const N: usize = 200_000;
        let order = dependency_order("t0", |n| {
            let i: usize = n.strip_prefix('t')?.parse().ok()?;
            Some(if i + 1 < N {
                vec![bare(&format!("t{}", i + 1))]
            } else {
                vec![]
            })
        })
        .unwrap();
        let order = step_names(&order);
        assert_eq!(order.len(), N);
        assert_eq!(*order.first().unwrap(), format!("t{}", N - 1)); // deepest runs first
        assert_eq!(*order.last().unwrap(), "t0"); // target runs last
    }
}
