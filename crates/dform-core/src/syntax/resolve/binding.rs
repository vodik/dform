//! What binds a variable in a body (R-10, docs/grammar.md "Bodies"). The
//! binders are exactly `x = term` with `x` unbound (a pattern on the left
//! binds its names), `x in term` for `x`, an aggregate (`n = count(x)`
//! binds `n`), and a relation atom's free variables. Every other operand
//! (a comparison's, an arithmetic operator's, a function's argument, an
//! attribute read, a `has` or a `not`) needs its variables bound by some
//! other literal of the body that does not itself depend on them; the
//! order the literals are written in is irrelevant. An unbound operand is
//! an error at it, and `=` with both sides bound by other literals is an
//! error that says to write `==`, so each spelling has one meaning.
//!
//! The check reads the source: which names are variables is the lowering's
//! answer (`Rc::vars`), what binds is the literal's form.

use super::*;

/// Where a variable is read: its name, where, and the operator that reads
/// it (`<`, `has`, a call of `f`, ..).
#[derive(Clone)]
struct Use {
    name: String,
    at: rowan::TextRange,
    offset: u32,
    by: String,
}

/// One way a literal can hold: the names it binds once `needs` are bound.
#[derive(Clone, Default)]
struct Mode {
    binds: Vec<String>,
    /// Names an index binds (`T[e].p`, `xs[i]`): the read is made before
    /// the literal, so the literal may read them too.
    keys: Vec<String>,
    needs: Vec<Use>,
}

impl Mode {
    fn holds(&self, bound: &BTreeSet<String>) -> bool {
        self.needs
            .iter()
            .all(|u| bound.contains(&u.name) || self.keys.contains(&u.name))
    }

    fn missing<'m>(&'m self, bound: &BTreeSet<String>) -> Option<&'m Use> {
        self.needs
            .iter()
            .find(|u| !bound.contains(&u.name) && !self.keys.contains(&u.name))
    }
}

/// A literal of the body: its modes, the bodies inside it checked against
/// what the body binds (`not { }`, a comprehension, a negated literal), and
/// for `x = t` the `=` token, which must bind.
struct Shape {
    modes: Vec<Mode>,
    inner: Vec<Inner>,
    eq: Option<SyntaxToken>,
    /// The names a negation or a comprehension in it shares with the body:
    /// it holds once the body binds them, whatever their order.
    waits: BTreeSet<String>,
}

impl Shape {
    /// The mode that holds once `bound` is, all of `waits` the body binds
    /// (`known`) among it.
    fn holds(&self, bound: &BTreeSet<String>, known: &BTreeSet<String>) -> Option<&Mode> {
        let m = self.modes.iter().find(|m| m.holds(bound))?;
        self.waits
            .iter()
            .all(|w| bound.contains(w) || !known.contains(w) || m.binds.contains(w))
            .then_some(m)
    }
}

enum Inner {
    /// A negated literal: what it reads must be bound outside it.
    Not(Vec<Use>),
    /// A body of its own, with an item read under it.
    Body(Vec<SyntaxNode>, Vec<SyntaxNode>, u32),
}

impl Lowerer<'_> {
    /// Check the literals of one body (a statement's clauses together)
    /// against what binds; `outer` are the source names bound around it.
    /// The order the literals are evaluated in: each after what binds what
    /// it reads, otherwise as written.
    pub(super) fn check_order(
        &mut self,
        rc: &Rc,
        lits: &[SyntaxNode],
        outer: &BTreeSet<String>,
    ) -> L<Vec<usize>> {
        if self.lenient || self.core {
            return Ok((0..lits.len()).collect());
        }
        let before = self.diags.len();
        let order = self.order(rc, lits, 0, outer);
        if self.diags.len() > before {
            Err(Skip)
        } else {
            Ok(order)
        }
    }

    fn order(
        &mut self,
        rc: &Rc,
        lits: &[SyntaxNode],
        offset: u32,
        outer: &BTreeSet<String>,
    ) -> Vec<usize> {
        let shapes: Vec<Shape> = lits.iter().map(|l| self.shape(rc, l, offset)).collect();
        // What some literal can bind: a name nothing binds is unknown.
        let mut known = outer.clone();
        for m in shapes.iter().flat_map(|s| &s.modes) {
            known.extend(m.binds.iter().chain(&m.keys).cloned());
        }
        let (bound, order) = fixpoint(&shapes, outer, &known, None);
        let mut fired = vec![false; shapes.len()];
        for &i in &order {
            fired[i] = true;
        }
        let mut reported = BTreeSet::new();
        for (i, s) in shapes.iter().enumerate() {
            if !fired[i] {
                let missing = s.modes[0].missing(&bound).cloned();
                if let Some(u) = missing
                    && reported.insert(u.name.clone())
                {
                    self.unbound(&u, &known);
                }
                continue;
            }
            // `x = t` with both sides bound by the other literals.
            if let Some(eq) = &s.eq {
                let (without, _) = fixpoint(&shapes, outer, &known, Some(i));
                if s.modes
                    .iter()
                    .all(|m| m.binds.iter().all(|b| without.contains(b)))
                {
                    let span = self.span_of(eq.text_range() + rowan::TextSize::from(offset));
                    self.diags.push(
                        Diagnostic::error(span, "both sides are bound; write `==`")
                            .with_help(
                                "`=` binds a name the body does not bind otherwise; `==` \
                                 compares two bound terms",
                            )
                            .with_fix("write `==`", vec![(span, "==".to_string())]),
                    );
                }
            }
        }
        for s in &shapes {
            for inner in &s.inner {
                match inner {
                    Inner::Not(needs) => {
                        if let Some(u) = needs.iter().find(|u| !bound.contains(&u.name))
                            && reported.insert(u.name.clone())
                        {
                            self.unbound(u, &known);
                        }
                    }
                    Inner::Body(lits, items, offset) => {
                        let shapes: Vec<Shape> =
                            lits.iter().map(|l| self.shape(rc, l, *offset)).collect();
                        let mut inner_known = bound.clone();
                        for m in shapes.iter().flat_map(|s| &s.modes) {
                            inner_known.extend(m.binds.iter().chain(&m.keys).cloned());
                        }
                        let (local, _) = fixpoint(&shapes, &bound, &inner_known, None);
                        self.order(rc, lits, *offset, &bound);
                        let mut item = Mode::default();
                        for t in items {
                            self.reads(rc, t, *offset, "in this item", &mut item, &mut Vec::new());
                        }
                        if let Some(u) = item.needs.iter().find(|u| !local.contains(&u.name))
                            && reported.insert(u.name.clone())
                        {
                            self.unbound(u, &known);
                        }
                    }
                }
            }
        }
        // The rest, as written, after what holds.
        let mut all = order;
        all.extend((0..shapes.len()).filter(|i| !fired[*i]));
        all
    }

    fn unbound(&mut self, u: &Use, known: &BTreeSet<String>) {
        let span = self.span_of(u.at + rowan::TextSize::from(u.offset));
        let name = &u.name;
        let quoted = format!("\"{name}\"");
        let d = if u.by == "at this `==`" {
            // `x == t` meant as a binding (R-10), or a string unquoted.
            Diagnostic::error(
                span,
                format!("`{name}` is unbound at this `==`; `=` binds, `==` compares"),
            )
            .with_help(format!(
                "bind it with `{name} = ..`, `in`, or a relation; a string is quoted: {quoted}"
            ))
        } else if !known.contains(name) {
            super::unknown_name(span, name, &quoted, true, known.iter().map(String::as_str))
        } else {
            Diagnostic::error(
                span,
                format!(
                    "`{name}` is unbound {}; bind it with `=`, `in`, or a relation first",
                    u.by
                ),
            )
        };
        self.diags.push(d);
    }

    fn is_var(rc: &Rc, name: &str) -> bool {
        rc.vars.contains_key(name)
    }

    /// What one literal binds and needs.
    fn shape(&mut self, rc: &Rc, n: &SyntaxNode, offset: u32) -> Shape {
        let mut shape = Shape {
            modes: Vec::new(),
            inner: Vec::new(),
            eq: None,
            waits: BTreeSet::new(),
        };
        let mut mode = Mode::default();
        let mut bodies = Vec::new();
        match n.kind() {
            LIT_ATOM => {
                if let Some(call) = terms(n).next() {
                    self.atom_shape(rc, &call, offset, &mut mode, &mut bodies);
                }
            }
            LIT_TRUTH => {
                for t in terms(n) {
                    self.reads(rc, &t, offset, "in this test", &mut mode, &mut bodies);
                }
            }
            LIT_HAS => {
                for t in terms(n) {
                    self.reads(rc, &t, offset, "at this `has`", &mut mode, &mut bodies);
                }
            }
            LIT_CMP => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let ops: Vec<SyntaxToken> = tokens(n).filter(|t| t.kind().is_cmp()).collect();
                if ts.len() == 2 && ops.len() == 1 && ops[0].kind() == EQ {
                    return self.eq_shape(rc, &ts, &ops[0], offset);
                }
                for (i, t) in ts.iter().enumerate() {
                    let op = ops.get(i.saturating_sub(1)).or(ops.first());
                    let by = op.map_or("here".to_string(), |o| format!("at this `{}`", o.text()));
                    self.reads(rc, t, offset, &by, &mut mode, &mut bodies);
                }
            }
            LIT_IN => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                if let Some(lhs) = ts.first() {
                    self.pattern_names(rc, lhs, offset, &mut mode, &mut bodies);
                }
                for t in ts.iter().skip(1) {
                    self.reads(rc, t, offset, "at this `in`", &mut mode, &mut bodies);
                }
            }
            LIT_NOT_IN => {
                for t in terms(n) {
                    self.reads(rc, &t, offset, "at this `not in`", &mut mode, &mut bodies);
                }
            }
            LIT_NOT => {
                if let Some(inner) = n.children().next() {
                    let s = self.shape(rc, &inner, offset);
                    let mut needs: Vec<Use> = s.modes[0].needs.clone();
                    for u in &mut needs {
                        u.by = "at this `not`".to_string();
                    }
                    // What it binds itself is its own, under the `not`.
                    let own: BTreeSet<&String> =
                        s.modes[0].binds.iter().chain(&s.modes[0].keys).collect();
                    needs.retain(|u| !own.contains(&u.name));
                    shape.inner.push(Inner::Not(needs));
                    shape.inner.extend(s.inner);
                }
            }
            LIT_NOT_BLOCK => {
                if let Some(b) = node(n, BODY) {
                    shape
                        .inner
                        .push(Inner::Body(b.children().collect(), Vec::new(), offset));
                }
            }
            _ => {}
        }
        if matches!(n.kind(), LIT_NOT | LIT_NOT_BLOCK) {
            names_in(rc, n, &mut shape.waits);
        }
        shared(rc, &bodies, &mut shape.waits);
        shape.inner.extend(bodies);
        shape.modes.push(mode);
        shape
    }

    /// `a = b`: a pattern side binds, given what the other side reads;
    /// `P = e[i]` binds the index too (H-9); `n = count(x)` binds `n`.
    fn eq_shape(&mut self, rc: &Rc, ts: &[SyntaxNode], eq: &SyntaxToken, offset: u32) -> Shape {
        let mut shape = Shape {
            modes: Vec::new(),
            inner: Vec::new(),
            eq: None,
            waits: BTreeSet::new(),
        };
        let mut bodies = Vec::new();
        let pattern = |t: &SyntaxNode| {
            matches!(t.kind(), TUPLE | OBJECT | LITERAL)
                || Chain::of(t).is_some_and(|c| c.is_bare())
        };
        for (p, v) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
            if !pattern(p) {
                continue;
            }
            let mut mode = Mode::default();
            self.pattern_names(rc, p, offset, &mut mode, &mut bodies);
            self.reads(rc, v, offset, "at this `=`", &mut mode, &mut bodies);
            shape.modes.push(mode);
        }
        if shape.modes.is_empty() {
            let mut mode = Mode::default();
            for t in ts {
                self.reads(rc, t, offset, "at this `=`", &mut mode, &mut bodies);
            }
            shape.modes.push(mode);
        }
        // `=` binds a name the other literals do not; a tuple or object
        // pattern may also compare (R-58).
        if !ts.iter().any(Self::is_pattern) {
            shape.eq = Some(eq.clone());
        }
        shared(rc, &bodies, &mut shape.waits);
        shape.inner.extend(bodies);
        shape
    }

    /// A relation's arguments bind; a function's (a predicate) and an
    /// extern's inputs are read.
    fn atom_shape(
        &mut self,
        rc: &Rc,
        call: &SyntaxNode,
        offset: u32,
        mode: &mut Mode,
        bodies: &mut Vec<Inner>,
    ) {
        let name = self.callee(call).unwrap_or_default();
        let by = format!("in this call of `{name}`");
        let function = crate::functions::callable(&name);
        let ext = self.decls.externs.get(&name).cloned();
        // `c[t].p(..)`, `platform[env=e].p(..)`: the index names a copy.
        if let Some(head) = call.children().find(|c| c.kind() == CHAIN) {
            for ix in head.children().filter(|c| c.kind() == INDEX) {
                for t in ix.children() {
                    let t = match t.kind() {
                        NAMED_ARG => terms(&t).next(),
                        _ => Some(t),
                    };
                    if let Some(t) = t {
                        self.pattern_names(rc, &t, offset, mode, bodies);
                    }
                }
            }
        }
        let Some(list) = node(call, ARG_LIST) else {
            return;
        };
        for (i, a) in list.children().enumerate() {
            let t = match a.kind() {
                NAMED_ARG => terms(&a).next(),
                k if is_term(k) => Some(a),
                _ => None,
            };
            let Some(t) = t else { continue };
            let input = ext
                .as_ref()
                .is_some_and(|cols| cols.get(i).is_some_and(|(input, _)| *input));
            if function || input {
                self.reads(rc, &t, offset, &by, mode, bodies);
            } else {
                self.pattern_names(rc, &t, offset, mode, bodies);
            }
        }
    }

    /// The names a pattern binds into `mode`, and what its non-pattern
    /// parts read.
    fn pattern_names(
        &mut self,
        rc: &Rc,
        t: &SyntaxNode,
        offset: u32,
        mode: &mut Mode,
        bodies: &mut Vec<Inner>,
    ) {
        match t.kind() {
            TUPLE | LIST => {
                for x in terms(t) {
                    self.pattern_names(rc, &x, offset, mode, bodies);
                }
            }
            OBJECT => {
                for f in t.children().filter(|c| c.kind() == OBJECT_FIELD) {
                    match terms(&f).next() {
                        Some(v) => self.pattern_names(rc, &v, offset, mode, bodies),
                        None => {
                            if let Some(k) = tokens(&f).next()
                                && Self::is_var(rc, k.text())
                            {
                                mode.binds.push(k.text().to_string());
                            }
                        }
                    }
                }
            }
            LITERAL if !tokens(t).any(|k| k.kind() == STRING && k.text().contains("${")) => {}
            CHAIN => match Chain::of(t) {
                Some(c) if c.is_bare() => {
                    if Self::is_var(rc, &c.head) {
                        mode.binds.push(c.head);
                    }
                }
                _ => self.reads(rc, t, offset, "in this read", mode, bodies),
            },
            _ => self.reads(rc, t, offset, "here", mode, bodies),
        }
    }

    /// The variables term `t` reads, each with what reads it.
    fn reads(
        &mut self,
        rc: &Rc,
        t: &SyntaxNode,
        offset: u32,
        by: &str,
        out: &mut Mode,
        bodies: &mut Vec<Inner>,
    ) {
        let read = |name: &str, at: rowan::TextRange, by: &str, out: &mut Mode| {
            if Self::is_var(rc, name) {
                out.needs.push(Use {
                    name: name.to_string(),
                    at,
                    offset,
                    by: by.to_string(),
                });
            }
        };
        match t.kind() {
            LITERAL => {
                for s in tokens(t).filter(|k| k.kind() == STRING) {
                    for (hole, at) in holes(s.text()) {
                        let at = u32::from(s.text_range().start()) + at as u32 + offset;
                        let p = parse::parse_term(hole);
                        for n in p.syntax().children() {
                            self.reads(rc, &n, at, "in this interpolation", out, bodies);
                        }
                    }
                }
            }
            CHAIN => {
                let c = Chain::of(t);
                let bare = c.as_ref().is_some_and(|c| c.is_bare());
                if let Some(h) = tokens(t).next() {
                    let by = if bare {
                        by.to_string()
                    } else {
                        format!("at this read `{}`", t.text())
                    };
                    read(h.text(), h.text_range(), &by, out);
                }
                for ix in t.children().filter(|c| c.kind() == INDEX) {
                    self.index(rc, &ix, offset, out, bodies);
                }
            }
            CALL => {
                let name = self.callee(t).unwrap_or_default();
                let by = format!("in this call of `{name}`");
                // What an aggregate folds is the body's (`unbound_aggregates`).
                if crate::partition::AGGREGATES.contains(&name.as_str()) {
                    return;
                }
                if let Some(list) = node(t, ARG_LIST) {
                    for a in list.children() {
                        let a = match a.kind() {
                            NAMED_ARG => terms(&a).next(),
                            _ => Some(a),
                        };
                        if let Some(a) = a {
                            self.reads(rc, &a, offset, &by, out, bodies);
                        }
                    }
                }
            }
            CALL_CHAIN => {
                for c in t.children() {
                    match c.kind() {
                        CALL => self.reads(rc, &c, offset, by, out, bodies),
                        INDEX => self.index(rc, &c, offset, out, bodies),
                        _ => {}
                    }
                }
            }
            OBJECT => {
                for f in t.children().filter(|c| c.kind() == OBJECT_FIELD) {
                    match terms(&f).next() {
                        Some(v) => self.reads(rc, &v, offset, by, out, bodies),
                        None => {
                            if let Some(k) = tokens(&f).next() {
                                read(k.text(), k.text_range(), by, out);
                            }
                        }
                    }
                }
            }
            BIN_EXPR => {
                let op = tokens(t)
                    .next()
                    .map(|o| o.text().to_string())
                    .unwrap_or_default();
                for x in terms(t) {
                    self.reads(rc, &x, offset, &format!("at this `{op}`"), out, bodies);
                }
            }
            UNARY_EXPR => {
                for x in terms(t) {
                    self.reads(rc, &x, offset, "at this `-`", out, bodies);
                }
            }
            // A comprehension's body binds its own names (checked after the
            // body around it).
            COMPREHENSION => {
                if let Some(b) = node(t, BODY) {
                    bodies.push(Inner::Body(
                        b.children().collect(),
                        terms(t).collect(),
                        offset,
                    ));
                }
            }
            _ => {
                for x in terms(t) {
                    self.reads(rc, &x, offset, by, out, bodies);
                }
            }
        }
    }
}

impl Lowerer<'_> {
    /// An index: a name alone binds, as the key of the row it reads (`T[e]`,
    /// `p[k]`, `xs[i]`); anything else is read.
    fn index(
        &mut self,
        rc: &Rc,
        ix: &SyntaxNode,
        offset: u32,
        out: &mut Mode,
        bodies: &mut Vec<Inner>,
    ) {
        for x in ix.children() {
            let x = match x.kind() {
                NAMED_ARG => terms(&x).next(),
                _ => Some(x),
            };
            let Some(x) = x else { continue };
            match Chain::of(&x) {
                Some(c) if c.is_bare() => {
                    if Self::is_var(rc, &c.head) {
                        out.keys.push(c.head);
                    }
                }
                _ => self.reads(rc, &x, offset, "in this index", out, bodies),
            }
        }
    }
}

/// The variables written in `n`: a chain's head, an object's shorthand
/// key, a name in an interpolation.
fn names_in(rc: &Rc, n: &SyntaxNode, out: &mut BTreeSet<String>) {
    let mut add = |name: &str| {
        if rc.vars.contains_key(name) {
            out.insert(name.to_string());
        }
    };
    for d in n.descendants() {
        match d.kind() {
            CHAIN => {
                if let Some(h) = tokens(&d).next() {
                    add(h.text());
                }
            }
            OBJECT_FIELD if terms(&d).next().is_none() => {
                if let Some(k) = tokens(&d).next() {
                    add(k.text());
                }
            }
            LITERAL => {
                for t in tokens(&d).filter(|t| t.kind() == STRING) {
                    for (hole, _) in holes(t.text()) {
                        let p = parse::parse_term(hole);
                        let mut inner = BTreeSet::new();
                        names_in(rc, &p.syntax(), &mut inner);
                        inner.iter().for_each(|x| add(x));
                    }
                }
            }
            _ => {}
        }
    }
}

/// The names the comprehensions among `bodies` share with the body.
fn shared(rc: &Rc, bodies: &[Inner], out: &mut BTreeSet<String>) {
    for b in bodies {
        if let Inner::Body(lits, items, _) = b {
            for x in lits.iter().chain(items) {
                names_in(rc, x, out);
            }
        }
    }
}

/// What the body binds from `outer`, and the literals that hold in the
/// order they hold: each time the first, as written, whose operands are
/// bound. `skip` leaves one literal out.
fn fixpoint(
    shapes: &[Shape],
    outer: &BTreeSet<String>,
    known: &BTreeSet<String>,
    skip: Option<usize>,
) -> (BTreeSet<String>, Vec<usize>) {
    let mut bound = outer.clone();
    let mut fired = vec![false; shapes.len()];
    let mut order = Vec::new();
    loop {
        let next = shapes.iter().enumerate().find_map(|(i, s)| {
            if fired[i] || Some(i) == skip {
                return None;
            }
            s.holds(&bound, known).map(|m| (i, m))
        });
        let Some((i, m)) = next else {
            return (bound, order);
        };
        bound.extend(m.binds.iter().cloned());
        bound.extend(m.keys.iter().cloned());
        fired[i] = true;
        order.push(i);
    }
}

/// The `${..}` holes of a string token's text, with the byte offset of
/// each hole's text in the token (`resolve::pieces`).
fn holes(text: &str) -> Vec<(&str, usize)> {
    super::pieces(text)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| match p {
            super::Piece::Hole(h, at) => Some((h, at)),
            super::Piece::Text(_) => None,
        })
        .collect()
}
