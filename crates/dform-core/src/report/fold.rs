//! An attribute as the source wrote it (R-124): a resource's leaves
//! grouped by the contribution that wrote each. A subtree whose leaves
//! one contribution wrote prints once, at the path where the writers
//! diverge, as one value in the formatter's layout
//! ([`crate::fmt::value`]); a leaf another contribution wrote splits out
//! at its own line. A leaf no contribution of the program wrote (a
//! schema's default of a merge key) does not split a value: it prints on
//! its own line beside it.

use crate::fmt::value::Tree;
use crate::partition::fmt_bare;
use crate::value::Value;
use std::collections::HashMap;

/// One step of a printed path: a key, or a list's selector, with the
/// text it is printed with (`.name`, `[name=traefik]`, `[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tok {
    pub step: Step,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// An object's field, as printed (quoted when it is not a name).
    Key(String),
    /// A positional element.
    Index(usize),
    /// A keyed element, `[name=traefik]`: its key fields.
    Keyed(Vec<(String, String)>),
    /// Any other selector (an element of a set before it is labeled).
    Other(String),
}

/// The steps of printed path `path` (`spec.containers[name=web].args[0]`).
pub fn tokens(path: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    for (i, seg) in crate::ir::path_segments(path).into_iter().enumerate() {
        let index = crate::ir::segment_parts(seg).1;
        let key = &seg[..seg.len() - index.len()];
        if !key.is_empty() {
            out.push(Tok {
                step: Step::Key(key.to_string()),
                text: match i {
                    0 => key.to_string(),
                    _ => format!(".{key}"),
                },
            });
        }
        let mut rest = index;
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let inner = &r[..end];
            let step = match inner.parse::<usize>() {
                Ok(n) => Step::Index(n),
                Err(_) if inner.contains('=') => Step::Keyed(
                    inner
                        .split(',')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                Err(_) => Step::Other(inner.to_string()),
            };
            out.push(Tok {
                step,
                text: format!("[{inner}]"),
            });
            rest = &r[end + 1..];
        }
    }
    out
}

/// The part of `v` at steps `toks`.
pub fn reach<'v>(v: &'v Value, toks: &[Tok]) -> Option<&'v Value> {
    let Some((t, rest)) = toks.split_first() else {
        return Some(v);
    };
    let next = match (&t.step, v) {
        (Step::Key(k), Value::Obj(m)) => m.get(crate::ir::segment_key(k).as_ref())?,
        (Step::Index(i), Value::List(xs)) => xs.get(*i)?,
        (Step::Keyed(pairs), Value::List(xs)) => xs.iter().find(|x| keyed(x, pairs))?,
        _ => return None,
    };
    reach(next, rest)
}

/// [`reach`] in a contribution's value `v`, `merged` the merged value
/// at the same place: an element by position is the contribution's
/// element equal to the merged list's at that position (contributions'
/// lists are joined), else by its own position when nothing is merged.
pub fn reach_along<'v>(v: &'v Value, merged: Option<&Value>, toks: &[Tok]) -> Option<&'v Value> {
    let mut cur = (v, merged);
    for t in toks {
        cur = step_along(cur.0, cur.1, t)?;
    }
    Some(cur.0)
}

/// What a prefix of paths into one value reached ([`reach_along_memo`]),
/// by the prefix's text.
pub type Reached<'v, 'm> = HashMap<String, Option<(&'v Value, Option<&'m Value>)>>;

/// [`reach_along`] of many paths into one value from one place: what each
/// prefix reaches is kept in `memo`, so a list's element is matched
/// against the merged list's once, not once per leaf under it.
pub fn reach_along_memo<'v, 'm>(
    memo: &mut Reached<'v, 'm>,
    v: &'v Value,
    merged: Option<&'m Value>,
    toks: &[Tok],
) -> Option<&'v Value> {
    let mut cur = (v, merged);
    let mut key = String::new();
    for t in toks {
        key.push('\0');
        key.push_str(&t.text);
        let next = match memo.get(&key) {
            Some(hit) => *hit,
            None => {
                let next = step_along(cur.0, cur.1, t);
                memo.insert(key.clone(), next);
                next
            }
        };
        cur = next?;
    }
    Some(cur.0)
}

/// One step of [`reach_along`].
fn step_along<'v, 'm>(
    v: &'v Value,
    merged: Option<&'m Value>,
    t: &Tok,
) -> Option<(&'v Value, Option<&'m Value>)> {
    Some(match (&t.step, v) {
        (Step::Key(k), Value::Obj(m)) => {
            let k = crate::ir::segment_key(k);
            let merged = match merged {
                Some(Value::Obj(mm)) => mm.get(k.as_ref()),
                _ => None,
            };
            (m.get(k.as_ref())?, merged)
        }
        (Step::Index(i), Value::List(xs)) => match merged {
            Some(Value::List(ms)) => {
                let m = ms.get(*i)?;
                (xs.iter().find(|x| *x == m)?, Some(m))
            }
            _ => (xs.get(*i)?, None),
        },
        (Step::Keyed(pairs), Value::List(xs)) => {
            let merged = match merged {
                Some(Value::List(ms)) => ms.iter().find(|x| keyed(x, pairs)),
                _ => None,
            };
            (xs.iter().find(|x| keyed(x, pairs))?, merged)
        }
        _ => return None,
    })
}

/// Whether element `x` has the key fields `pairs` (`name=web`); a field
/// it leaves out is the schema's default, and does not tell.
pub fn keyed(x: &Value, pairs: &[(String, String)]) -> bool {
    let Value::Obj(m) = x else { return false };
    pairs
        .iter()
        .all(|(k, want)| m.get(k).is_none_or(|v| fmt_bare(v) == *want))
}

/// Where the leaf at `toks` sits in value `v`, to order leaves by: a
/// key by its name, an element by its position in the list (a keyed
/// one's in the order the program gave the elements).
pub fn position(v: &Value, toks: &[Tok]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut at = Some(v);
    for t in toks {
        let (pos, next) = match (&t.step, at) {
            (Step::Key(k), Some(Value::Obj(m))) => (0, m.get(crate::ir::segment_key(k).as_ref())),
            (Step::Index(i), Some(Value::List(xs))) => (*i, xs.get(*i)),
            (Step::Keyed(pairs), Some(Value::List(xs))) => {
                match xs.iter().position(|x| keyed(x, pairs)) {
                    Some(i) => (i, xs.get(i)),
                    None => (usize::MAX, None),
                }
            }
            (Step::Index(i), _) => (*i, None),
            _ => (0, None),
        };
        out.push((pos, t.text.clone()));
        at = next;
    }
    out
}

/// One printed item: the leaves it prints, at `path`. A fold of one leaf
/// is that leaf's own line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub path: String,
    pub depth: usize,
    pub leaves: Vec<usize>,
}

/// Fold leaves `paths`, each written by `writers[i]` (`None`: by no
/// contribution of the program), into the items to print, in the order
/// of their first leaf: each writer's leaves as one value at the deepest
/// path that holds them all, where it and the other writers diverge
/// (`spec.template.spec = { .. }`, `tags = { .. }` beside a policy's
/// `tags.team`); a writer of one leaf, and a leaf no one wrote, on its
/// own line.
pub fn fold<W: PartialEq>(paths: &[String], writers: &[Option<W>]) -> Vec<Group> {
    let toks: Vec<Vec<Tok>> = paths.iter().map(|p| tokens(p)).collect();
    let alone = |i: usize| Group {
        path: paths[i].clone(),
        depth: toks[i].len(),
        leaves: vec![i],
    };
    let mut out = Vec::new();
    let mut done = vec![false; paths.len()];
    for i in 0..paths.len() {
        if done[i] {
            continue;
        }
        let Some(w) = &writers[i] else {
            done[i] = true;
            out.push(alone(i));
            continue;
        };
        let mine: Vec<usize> = (i..paths.len())
            .filter(|&j| !done[j] && writers[j].as_ref() == Some(w))
            .collect();
        mine.iter().for_each(|&j| done[j] = true);
        // The steps every leaf of the writer shares, short of a leaf.
        let shortest = mine.iter().map(|&j| toks[j].len()).min().unwrap_or(0);
        let mut depth = (0..shortest.saturating_sub(1))
            .take_while(|&d| mine.iter().all(|&j| toks[j][d].step == toks[i][d].step))
            .count();
        // A list's element by position is the list's: `egress = [{ .. }]`.
        while depth > 0 && matches!(toks[i][depth - 1].step, Step::Index(_) | Step::Other(_)) {
            depth -= 1;
        }
        // A list other writers add elements to as well: each of this
        // writer's elements by its position (`statements[1] = { .. }`),
        // not a list that says only part of it.
        let positional = matches!(
            toks[i].get(depth).map(|t| &t.step),
            Some(Step::Index(_) | Step::Other(_))
        );
        let shared = positional
            && (0..paths.len()).any(|j| {
                writers[j].as_ref() != Some(w)
                    && toks[j].len() > depth
                    && toks[j][..depth] == toks[i][..depth]
            });
        if shared && depth > 0 {
            let mut elements: Vec<Vec<usize>> = Vec::new();
            for &j in &mine {
                match elements
                    .iter_mut()
                    .find(|e| toks[e[0]][depth].step == toks[j][depth].step)
                {
                    Some(e) => e.push(j),
                    None => elements.push(vec![j]),
                }
            }
            for e in elements {
                match e.as_slice() {
                    [j] => out.push(alone(*j)),
                    _ => out.push(Group {
                        path: text(&toks[e[0]], depth + 1),
                        depth: depth + 1,
                        leaves: e,
                    }),
                }
            }
            continue;
        }
        match (mine.as_slice(), depth) {
            // A list only this writer wrote, of one element: the list
            // as it was written (`policies = [app_policy]`).
            ([j], d) if d > 0 && positional => out.push(Group {
                path: text(&toks[*j], d),
                depth: d,
                leaves: vec![*j],
            }),
            ([j], _) => out.push(alone(*j)),
            (_, 0) => out.extend(mine.iter().map(|&j| alone(j))),
            _ => out.push(Group {
                path: text(&toks[i], depth),
                depth,
                leaves: mine,
            }),
        }
    }
    // A value where its path's leaves begin, before a leaf another
    // writer made inside it: `metadata = { .. }`, then
    // `metadata.labels.owner = ..`.
    out.sort_by_key(|g| {
        let first = g.leaves.first().copied().unwrap_or_default();
        let at = (0..paths.len())
            .find(|&j| toks[j].len() >= g.depth && toks[j][..g.depth] == toks[first][..g.depth])
            .unwrap_or(first);
        (at, g.depth, first)
    });
    out
}

/// The printed path of the first `n` steps of `toks`.
fn text(toks: &[Tok], n: usize) -> String {
    toks[..n].iter().map(|t| t.text.as_str()).collect()
}

/// The value of group `g`: its leaves' values (`values[i]` of leaf
/// `paths[i]`) below its path, objects by key in the order they come,
/// a list's elements by position.
pub fn assemble(g: &Group, paths: &[String], values: &[Tree]) -> Tree {
    let rel: Vec<(Vec<Tok>, &Tree)> = g
        .leaves
        .iter()
        .map(|&i| (tokens(&paths[i])[g.depth..].to_vec(), &values[i]))
        .collect();
    build(&rel)
}

/// Leaves below a path: the steps left, and each leaf's value.
type Below<'a> = Vec<(Vec<Tok>, &'a Tree)>;

fn build(rel: &[(Vec<Tok>, &Tree)]) -> Tree {
    if let [(t, v)] = rel
        && t.is_empty()
    {
        return (*v).clone();
    }
    let mut children: Vec<(&Tok, Below)> = Vec::new();
    for (t, v) in rel {
        let Some((first, rest)) = t.split_first() else {
            continue;
        };
        let item = (rest.to_vec(), *v);
        match children.iter_mut().find(|(s, _)| s.step == first.step) {
            Some((_, xs)) => xs.push(item),
            None => children.push((first, vec![item])),
        }
    }
    let keyed = children.iter().all(|(t, _)| matches!(t.step, Step::Key(_)));
    if keyed {
        return Tree::Obj(
            children
                .iter()
                .map(|(t, xs)| match &t.step {
                    Step::Key(k) => (k.clone(), build(xs)),
                    _ => unreachable!("every step is a key"),
                })
                .collect(),
        );
    }
    children.sort_by_key(|(t, _)| match t.step {
        Step::Index(n) => n,
        _ => 0,
    });
    Tree::List(children.iter().map(|(_, xs)| build(xs)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn a_path_is_keys_and_selectors() {
        let t = tokens("spec.containers[name=web].args[0]");
        let steps: Vec<&Step> = t.iter().map(|t| &t.step).collect();
        assert_eq!(
            steps,
            [
                &Step::Key("spec".into()),
                &Step::Key("containers".into()),
                &Step::Keyed(vec![("name".into(), "web".into())]),
                &Step::Key("args".into()),
                &Step::Index(0),
            ]
        );
        let back: String = t.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(back, "spec.containers[name=web].args[0]");
    }

    #[test]
    fn one_writer_folds_where_the_writers_diverge() {
        let ps = paths(&[
            "metadata.labels.owner",
            "metadata.name",
            "spec.c[name=t].args[0]",
            "spec.c[name=t].args[1]",
            "spec.c[name=t].name",
            "spec.c[name=t].ports[name=web].name",
            "spec.c[name=t].ports[name=web].protocol",
        ]);
        let w = [Some(2), Some(3), Some(1), Some(1), Some(1), Some(1), None];
        let g = fold(&ps, &w);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(
            printed,
            [
                "metadata.labels.owner",
                "metadata.name",
                "spec.c[name=t]",
                "spec.c[name=t].ports[name=web].protocol",
            ]
        );
        let values: Vec<Tree> = (0..ps.len()).map(|i| Tree::Leaf(format!("v{i}"))).collect();
        assert_eq!(
            assemble(&g[2], &ps, &values),
            Tree::Obj(vec![
                (
                    "args".into(),
                    Tree::List(vec![Tree::Leaf("v2".into()), Tree::Leaf("v3".into())])
                ),
                ("name".into(), Tree::Leaf("v4".into())),
                (
                    "ports".into(),
                    Tree::List(vec![Tree::Obj(vec![(
                        "name".into(),
                        Tree::Leaf("v5".into())
                    )])])
                ),
            ])
        );
    }

    #[test]
    fn a_writer_folds_beside_another_in_the_same_object() {
        let ps = paths(&["tags.component", "tags.env", "tags.team"]);
        let g = fold(&ps, &[Some(1), Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["tags", "tags.team"]);
        // A value comes before a leaf another writer made inside it.
        let ps = paths(&[
            "metadata.labels.owner",
            "metadata.name",
            "metadata.namespace",
        ]);
        let g = fold(&ps, &[Some(2), Some(1), Some(1)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["metadata", "metadata.labels.owner"]);
    }

    #[test]
    fn a_list_several_writers_add_to_is_said_by_element() {
        let ps = paths(&[
            "statements[0].action",
            "statements[0].resource",
            "statements[1].action",
            "statements[1].resource",
            "statements[2].action",
        ]);
        let g = fold(&ps, &[Some(2), Some(2), Some(1), Some(1), Some(1)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(
            printed,
            ["statements[0]", "statements[1]", "statements[2].action"]
        );
    }

    #[test]
    fn a_list_of_one_element_one_writer_wrote_is_the_list() {
        let ps = paths(&["policies[0]", "tags.team"]);
        let g = fold(&ps, &[Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["policies", "tags.team"]);
        let values = [Tree::Leaf("p".into()), Tree::Leaf("t".into())];
        assert_eq!(
            assemble(&g[0], &ps, &values),
            Tree::List(vec![Tree::Leaf("p".into())])
        );
        // Another writer's element beside it: each by its own line.
        let g = fold(&paths(&["policies[0]", "policies[1]"]), &[Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["policies[0]", "policies[1]"]);
    }

    #[test]
    fn positions_order_numerically() {
        let ps = paths(&["a[10]", "a[2]"]);
        let g = fold(&ps, &[Some(1), Some(1)]);
        let values = [Tree::Leaf("ten".into()), Tree::Leaf("two".into())];
        assert_eq!(g[0].path, "a");
        assert_eq!(
            assemble(&g[0], &ps, &values),
            Tree::List(vec![Tree::Leaf("two".into()), Tree::Leaf("ten".into())])
        );
    }

    #[test]
    fn leaves_no_contribution_wrote_stay_their_own() {
        let ps = paths(&["a.b", "a.c"]);
        let g = fold::<u32>(&ps, &[None, None]);
        assert_eq!(g.len(), 2);
    }
}
