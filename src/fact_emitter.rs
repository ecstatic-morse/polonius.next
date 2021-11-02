// Tests dedicated to specific relations
#[cfg(test)]
mod test;

// Tests porting the existing examples using the manual fact format, to the new frontend format
#[cfg(test)]
mod examples;

use crate::ast::*;
use crate::ast_parser::parse_ast;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::ControlFlow;

#[derive(Default, PartialEq, Eq, Clone)]
struct Origin(String);

#[derive(Default, PartialEq, Eq, Clone)]
struct Node(String);

impl<S> From<S> for Origin
where
    S: AsRef<str> + ToString,
{
    fn from(s: S) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl<S> From<S> for Node
where
    S: AsRef<str> + ToString,
{
    fn from(s: S) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[derive(Default, Debug)]
pub(crate) struct Facts {
    access_origin: Vec<(Origin, Node)>,
    cfg_edge: Vec<(Node, Node)>,
    clear_origin: Vec<(Origin, Node)>,
    introduce_subset: Vec<(Origin, Origin, Node)>,
    invalidate_origin: Vec<(Origin, Node)>,
    node_text: Vec<(String, Node)>,
}

#[allow(dead_code)]
fn emit_facts(input: &str) -> eyre::Result<Facts> {
    let program = parse_ast(input)?;
    let emitter = FactEmitter::new(program, input, false);
    let mut facts = Default::default();
    emitter.emit_facts(&mut facts);
    Ok(facts)
}

// An internal representation of a `Node`, a location in the CFG: the block within the program,
// and the statement within that block. Used to analyze locations (e.g. reachability), whereas
// `Node`s are user-readable representations for facts.
#[allow(dead_code)]
struct Location {
    block_idx: usize,
    statement_idx: usize,
}

impl From<(usize, usize)> for Location {
    fn from((block_idx, statement_idx): (usize, usize)) -> Self {
        Self {
            block_idx,
            statement_idx,
        }
    }
}

struct FactEmitter<'a> {
    input: &'a str,
    program: Program,
    loans: HashMap<Place, Vec<(Origin, Location)>>,
    simple_node_names: bool,
}

impl<'a> FactEmitter<'a> {
    fn new(program: Program, input: &'a str, simple_node_names: bool) -> Self {
        // Collect loans from borrow expressions present in the program
        let mut loans: HashMap<Place, Vec<(Origin, Location)>> = HashMap::new();

        for (block_idx, bb) in program.basic_blocks.iter().enumerate() {
            for (statement_idx, s) in bb.statements.iter().enumerate() {
                let (Statement::Assign(_, expr) | Statement::Expr(expr)) = &**s;

                if let Expr::Access {
                    kind: AccessKind::Borrow(origin) | AccessKind::BorrowMut(origin),
                    place,
                } = expr
                {
                    // TODO: handle fields and loans taken on subsets of their paths.
                    // Until then: only support borrowing from complete places.
                    //
                    // TODO: we probably also need to track the loan's mode, if we want to emit
                    // errors when mutably borrowing through a shared ref and the likes ?
                    loans
                        .entry(place.clone())
                        .or_default()
                        .push((origin.into(), (block_idx, statement_idx).into()));
                }
            }
        }

        Self {
            input,
            program,
            loans,
            simple_node_names,
        }
    }

    fn emit_facts(&self, facts: &mut Facts) {
        for bb in &self.program.basic_blocks {
            self.emit_block_facts(bb, facts);
        }
    }

    fn emit_block_facts(&self, bb: &BasicBlock, facts: &mut Facts) {
        // Emit CFG facts for the block
        self.emit_cfg_edges(&bb, facts);

        for (idx, s) in bb.statements.iter().enumerate() {
            let node = self.node_at(&bb.name, idx);

            // Emit `node_text` for this statement: the line from where it was parsed
            // in the original input program.
            let statement_text = {
                let span = s.span();
                self.input[span.start()..span.end() - 1].to_string()
            };
            facts.node_text.push((statement_text, node.clone()));

            match &**s {
                Statement::Assign(place, expr) => {
                    // Emit facts about the assignment LHS
                    let lhs_ty = self.ty_of_place(place);
                    let lhs_origins = self.origins_of_place(place);

                    // Assignments clear all origins in the type
                    for origin in &lhs_origins {
                        facts.clear_origin.push((origin.clone(), node.clone()));
                    }

                    // TODO: the following is wrong and simplistic, see
                    // https://github.com/nikomatsakis/polonius.next/pull/4#discussion_r739325010
                    // but will be fixed by https://github.com/nikomatsakis/polonius.next/pull/10
                    if !lhs_ty.is_ref() {
                        // Assignments to non-references invalidate loans borrowing from them.
                        //
                        // TODO: handle assignments to fields and loans taken on subsets of
                        // their paths. Until then: only support invalidations on assignments
                        // to complete places.
                        //
                        if let Some(loans) = self.loans.get(place) {
                            for (origin, _location) in loans {
                                // TODO: if the `location` where the loan was issued can't
                                // reach the current location, there is no need to emit
                                // the invalidation
                                facts.invalidate_origin.push((origin.clone(), node.clone()));
                            }
                        }
                    }

                    // Emit facts about the assignment RHS: evaluate the `expr`
                    self.emit_expr_facts(&node, expr, facts);

                    // Relate the LHS and RHS tys
                    self.emit_subset_facts(&node, &lhs_ty, expr, facts);
                }

                Statement::Expr(expr) => {
                    // Evaluate the `expr`
                    self.emit_expr_facts(&node, expr, facts);
                }
            }
        }
    }

    fn emit_expr_facts(&self, node: &Node, expr: &Expr, facts: &mut Facts) {
        match expr {
            Expr::Access { kind, place } => {
                match kind {
                    // Borrowing clears its origin: it's issuing a fresh origin of the same name
                    AccessKind::Borrow(origin) | AccessKind::BorrowMut(origin) => {
                        facts.clear_origin.push((origin.into(), node.clone()));

                        if matches!(kind, AccessKind::BorrowMut(_)) {
                            // A mutable borrow is considered a write to the place:
                            //
                            // 1) it accesses the origins in the type
                            let origins = self.origins_of_place(place);
                            for origin in origins {
                                facts.access_origin.push((origin.clone(), node.clone()));
                            }

                            // 2) and invalidates existing loans of that place
                            //
                            // TODO: handle assignments to fields and loans taken on subsets of
                            // their paths. Until then: only support invalidations on assignments
                            // to complete places.
                            //
                            // TODO: here as well, there is a question of: can the loans we're
                            // invalidating, reach the current node ?
                            //
                            if let Some(loans) = self.loans.get(place) {
                                for (origin, _) in loans {
                                    facts.invalidate_origin.push((origin.clone(), node.clone()));
                                }
                            }
                        }
                    }

                    AccessKind::Copy | AccessKind::Move => {
                        // FIXME: currently function call parameters are not parsed without access
                        // kinds, check if there's some special behaviour needed for copy/moves,
                        // instead of just being "reads" (e.g. maybe moves also need clearing
                        // or invalidations)

                        // Reads access all the origins in their type
                        let origins = self.origins_of_place(place);
                        for origin in origins {
                            facts.access_origin.push((origin.into(), node.clone()));
                        }
                    }
                }
            }

            Expr::Call { arguments, .. } => {
                // Calls evaluate their arguments
                arguments
                    .iter()
                    .for_each(|expr| self.emit_expr_facts(&node, expr, facts));

                // TODO: Depending on the signature of the function, some subsets can be introduced
                // between the arguments to the call
            }

            _ => {}
        }
    }

    // Introduce subsets: `expr` flows into `place`
    //
    // TODO: do we need some type checking to ensure this assigment is valid
    // with respect to the LHS/RHS types, mutability, etc ?
    //
    // TODO: Complete this.
    //
    // We're in an assignment and we assume the LHS and RHS have the same shape,
    // for example `&'a Type<&'b i32> = &'1 Type<'2 i32>`.
    //
    fn emit_subset_facts(&self, node: &Node, lhs_ty: &Ty, rhs_expr: &Expr, facts: &mut Facts) {
        // Subset relationships are computed with respect to the variance rules.
        // https://doc.rust-lang.org/reference/subtyping.html#variance
        //
        // In the context of an assignment, the subsets follow the flow of data, and origins on the
        // RHS will flow into the ones on the LHS.
        //
        // We don't support function types in structs or function parameters at the moment, so
        // there's no contravariant relationships yet.

        match (lhs_ty, rhs_expr) {
            // `lhs = &rhs`, where lhs is a shared reference type
            (
                Ty::Ref {
                    origin: target_origin,
                    ty: lhs_ty,
                },
                Expr::Access {
                    kind: AccessKind::Borrow(source_origin),
                    place,
                },
            ) => {
                facts.introduce_subset.push((
                    source_origin.into(),
                    target_origin.into(),
                    node.clone(),
                ));
                let rhs_ty = self.ty_of_place(place);
                self.relate_tys(node, lhs_ty, rhs_ty, Variance::Covariant, facts);
            }

            // `lhs = copy or move rhs`, where lhs and rhs are shared reference types
            (
                Ty::Ref {
                    origin: target_origin,
                    ty: lhs_ty,
                },
                Expr::Access {
                    kind: AccessKind::Copy | AccessKind::Move,
                    place,
                },
            ) => {
                let rhs_ty = self.ty_of_place(place);
                match rhs_ty {
                    Ty::Ref {
                        origin: source_origin,
                        ty: rhs_ty,
                    } => {
                        facts.introduce_subset.push((
                            source_origin.into(),
                            target_origin.into(),
                            node.clone(),
                        ));
                        self.relate_tys(node, lhs_ty, rhs_ty, Variance::Covariant, facts);
                    }

                    _ => {
                        unreachable!(
                            "Can't relate LHS shared ref {:?}, and RHS {:?}",
                            lhs_ty, rhs_ty
                        )
                    }
                }
            }

            // `lhs = &mut rhs`, where lhs is a unique reference type
            (
                Ty::RefMut {
                    origin: target_origin,
                    ty: lhs_ty,
                },
                Expr::Access {
                    kind: AccessKind::BorrowMut(source_origin),
                    place,
                },
            ) => {
                facts.introduce_subset.push((
                    source_origin.into(),
                    target_origin.into(),
                    node.clone(),
                ));
                let rhs_ty = self.ty_of_place(place);
                self.relate_tys(node, lhs_ty, rhs_ty, Variance::Invariant, facts);
            }

            // `lhs = copy or move rhs`, where lhs and rhs are unique reference types
            (
                Ty::RefMut {
                    origin: target_origin,
                    ty: lhs_ty,
                },
                Expr::Access {
                    kind: AccessKind::Copy | AccessKind::Move,
                    place,
                },
            ) => {
                let rhs_ty = self.ty_of_place(place);
                match rhs_ty {
                    Ty::RefMut {
                        origin: source_origin,
                        ty: rhs_ty,
                    } => {
                        facts.introduce_subset.push((
                            source_origin.into(),
                            target_origin.into(),
                            node.clone(),
                        ));
                        self.relate_tys(node, lhs_ty, rhs_ty, Variance::Invariant, facts);
                    }

                    _ => {
                        unreachable!(
                            "Can't relate LHS unique ref {:?}, and RHS {:?}",
                            lhs_ty, rhs_ty
                        )
                    }
                }
            }

            // `lhs = rhs`, where lhs and rhs are structs, and may have generic parameters which
            // will need subsets.
            (
                Ty::Struct { .. },
                Expr::Access {
                    kind: AccessKind::Copy | AccessKind::Move,
                    place,
                },
            ) => {
                let rhs_ty = self.ty_of_place(place);
                self.relate_tys(node, lhs_ty, rhs_ty, Variance::Covariant, facts);
            }

            (_, Expr::Call { .. }) => {
                // TODO: When possible, check if the function signature requires that the RHS inputs
                // flow into the LHS output.
            }

            _ => {
                // Sanity check: all origins must have been processed in the arms above.
                // If this assert triggers when adding new tests or examples, then
                // a pattern is missing above.
                self.assert_no_origins_are_present(lhs_ty, rhs_expr);
            }
        }
    }

    // Emit subset relationships between the two types' parameters, according to the
    // variance rules, recursively.
    fn relate_tys(
        &self,
        node: &Node,
        lhs_ty: &Ty,
        rhs_ty: &Ty,
        variance: Variance,
        facts: &mut Facts,
    ) {
        match (lhs_ty, rhs_ty) {
            (
                Ty::Struct {
                    parameters: lhs_args,
                    ..
                },
                Ty::Struct {
                    parameters: rhs_args,
                    ..
                },
            ) => {
                // Relate the arguments to the generic structs pair-wise, according to variance
                for (lhs_arg, rhs_arg) in lhs_args.iter().zip(rhs_args.iter()) {
                    match (lhs_arg, rhs_arg) {
                        (
                            Parameter::Ty(
                                param @ Ty::Ref {
                                    origin: target_origin,
                                    ty: lhs_ty,
                                },
                            ),
                            Parameter::Ty(Ty::Ref {
                                origin: source_origin,
                                ty: rhs_ty,
                            }),
                        )
                        | (
                            Parameter::Ty(
                                param @ Ty::RefMut {
                                    origin: target_origin,
                                    ty: lhs_ty,
                                },
                            ),
                            Parameter::Ty(Ty::RefMut {
                                origin: source_origin,
                                ty: rhs_ty,
                            }),
                        ) => {
                            if let Variance::Covariant | Variance::Invariant = variance {
                                facts.introduce_subset.push((
                                    source_origin.into(),
                                    target_origin.into(),
                                    node.clone(),
                                ));
                            }

                            if let Variance::Contravariant | Variance::Invariant = variance {
                                facts.introduce_subset.push((
                                    target_origin.into(),
                                    source_origin.into(),
                                    node.clone(),
                                ));
                            }

                            // Unique references change the relationships of their children
                            // parameter pairs: they must be invariant.
                            let variance = if matches!(param, Ty::RefMut { .. }) {
                                Variance::Invariant
                            } else {
                                variance
                            };

                            self.relate_tys(node, &lhs_ty, &rhs_ty, variance, facts);
                        }

                        (Parameter::Ty(lhs_ty), Parameter::Ty(rhs_ty)) => {
                            // TODO: variance can also change if the type is special here:
                            // e.g. UnsafeCell
                            self.relate_tys(node, &lhs_ty, &rhs_ty, variance, facts);
                        }

                        _ => todo!(),
                    }
                }
            }

            _ => {}
        }
    }

    fn emit_cfg_edges(&self, bb: &BasicBlock, facts: &mut Facts) {
        let statement_count = bb.statements.len();

        // Emit intra-block CFG edges between statements
        for idx in 1..statement_count {
            facts
                .cfg_edge
                .push((self.node_at(&bb.name, idx - 1), self.node_at(&bb.name, idx)));
        }

        // Emit inter-block CFG edges between a block and its successors
        for succ in &bb.successors {
            // Note: `goto`s are not statements, so a block with a single goto
            // has no statements but still needs a node index in the CFG.
            facts.cfg_edge.push((
                self.node_at(&bb.name, statement_count.saturating_sub(1)),
                self.node_at(succ, 0),
            ));
        }
    }

    fn ty_of_place(&self, place: &Place) -> &Ty {
        self.walk_place_tys(place, |_| ())
    }

    fn origins_of_place(&self, place: &Place) -> Vec<Origin> {
        let mut origins = Vec::new();
        self.walk_place_tys(place, |ty| {
            ty.collect_origins_into(&mut origins);
        });
        origins
    }

    fn walk_place_tys<F>(&self, place: &Place, mut ty_walked_callback: F) -> &Ty
    where
        F: FnMut(&Ty),
    {
        // The `base` is always a variable of the program, but can be deref'd.
        let base = if let Some(deref_base) = place.deref_base() {
            deref_base
        } else {
            &place.base
        };

        let v = self
            .program
            .variables
            .iter()
            .find(|v| v.name == base)
            .unwrap_or_else(|| panic!("Can't find variable {}", place.base));

        let ty = if place.fields.is_empty() {
            &v.ty
        } else {
            // If there are any fields, then this must be a struct
            assert!(matches!(v.ty, Ty::Struct { .. }));

            // Find the type of each field in sequence, to return the last field's type
            place.fields.iter().fold(&v.ty, |ty, field_name| {
                // Notify a traversal step was taken for the current field parent's ty
                ty_walked_callback(ty);

                // Find the struct decl for the parent's ty
                let (struct_name, struct_substs) = match ty {
                    Ty::Struct { name, parameters } => (name, parameters),
                    _ => panic!("Ty {:?} must be a struct to access its fields", ty),
                };
                let decl = self
                    .program
                    .struct_decls
                    .iter()
                    .find(|s| &s.name == struct_name)
                    .unwrap_or_else(|| {
                        panic!("Can't find struct {} at field {}", struct_name, field_name,)
                    });

                // Find the expected named field inside the struct decl
                let field = decl
                    .field_decls
                    .iter()
                    .find(|v| &v.name == field_name)
                    .unwrap_or_else(|| {
                        panic!("Can't find field {} in struct {}", field_name, struct_name)
                    });

                // It's possible that the field has a generic type, which we need to substitute
                // with the matching type from the struct's arguments
                match &field.ty {
                    Ty::Struct {
                        name: field_ty_name,
                        ..
                    } => {
                        if let Some(idx) = decl.generic_decls.iter().position(|d| match d {
                            GenericDecl::Ty(param_ty_name) => param_ty_name == field_ty_name,
                            _ => false,
                        }) {
                            // We found the field ty in the generic decls, so return the subst
                            // at the same index
                            match &struct_substs[idx] {
                                Parameter::Ty(subst_ty) => subst_ty,

                                // TODO: handle generic origins
                                _ => panic!("The parameter at idx {} should be a Ty", idx),
                            }
                        } else {
                            // Otherwise, the field ty is a regular type
                            &field.ty
                        }
                    }
                    _ => &field.ty,
                }
            })
        };

        // Notify a step in the walk was taken, either:
        // - the `base` ty, when there are no fields
        // - the last field's ty, from the place's `fields` list. The callbacks for the previous
        // fields in the list have already been processed in the loop just above.
        ty_walked_callback(ty);

        ty
    }

    fn node_at(&self, block: &str, statement_idx: usize) -> Node {
        let mut node = format!("{}[{}]", block, statement_idx);

        // Hack: if we temporarily need simpler node names, while comparing to the manual facts:
        // use single-letter names.
        let use_simple_node_names = std::env::var("SIMPLE_NODES").is_ok();

        if self.simple_node_names || use_simple_node_names {
            // Make the block-local statement idx refer to a concatenated list of all
            // statements: adding the number of statements prior to this block.
            // (Here as well, count as if there's always at least one statement per block,
            // to account for empty blocks with a goto)
            let bb_statement_start_idx = self
                .program
                .basic_blocks
                .iter()
                .take_while(|bb| block != bb.name)
                .fold(0, |acc, bb| acc + bb.statements.len().max(1));
            let node_idx = 'a' as u32 + (bb_statement_start_idx + statement_idx) as u32;
            let node_as_letter = char::from_u32(node_idx).unwrap_or_else(|| {
                panic!(
                    "Couldn't turn '{}' into a single letter name for node {:?}",
                    node_idx, node
                )
            });
            node = node_as_letter.to_string().into()
        }

        node.into()
    }

    // Sanity check that no origins are present
    // - in the LHS ty
    // - in borrow expressions on the RHS
    // - in moves/copies of the RHS ty
    fn assert_no_origins_are_present(&self, lhs_ty: &Ty, rhs_expr: &Expr) {
        assert_eq!(
            lhs_ty.has_origins(),
            false,
            "LHS {:?} has unprocess origins, RHS: {:?}",
            lhs_ty,
            rhs_expr
        );

        if let Expr::Access { kind, place } = rhs_expr {
            assert_eq!(
                matches!(
                    kind,
                    AccessKind::Borrow { .. } | AccessKind::BorrowMut { .. }
                ),
                false,
                "RHS {:?} has unprocessed origins, LHS: {:?}",
                rhs_expr,
                lhs_ty,
            );

            match kind {
                AccessKind::Borrow { .. } | AccessKind::BorrowMut { .. } => {
                    panic!(
                        "RHS {:?} has unprocessed origins, LHS: {:?}",
                        rhs_expr, lhs_ty,
                    );
                }

                AccessKind::Copy | AccessKind::Move => {
                    let rhs_ty = self.ty_of_place(place);
                    assert_eq!(
                        rhs_ty.has_origins(),
                        false,
                        "RHS {:?} has unprocessed origins, LHS: {:?}",
                        rhs_ty,
                        lhs_ty,
                    );
                }
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Variance {
    Covariant,

    #[allow(dead_code)]
    Contravariant,

    Invariant,
}

trait TyVisitor {
    fn on_origin_visited(&mut self, origin: &Name) -> ControlFlow<()>;
}

impl Ty {
    fn is_ref(&self) -> bool {
        matches!(self, Ty::Ref { .. } | Ty::RefMut { .. })
    }

    // Returns true if this type contains origins, recursively.
    fn has_origins(&self) -> bool {
        struct OriginVisitor;
        impl TyVisitor for OriginVisitor {
            fn on_origin_visited(&mut self, _origin: &Name) -> ControlFlow<()> {
                ControlFlow::Break(())
            }
        }
        let mut visitor = OriginVisitor;
        self.visit_origins(&mut visitor).is_some()
    }

    // Visits all the origins present in this type, recursively.
    fn visit_origins<V>(&self, visitor: &mut V) -> Option<()>
    where
        V: TyVisitor,
    {
        match self {
            Ty::Ref { origin, ty } | Ty::RefMut { origin, ty } => {
                if let ControlFlow::Break(value) = visitor.on_origin_visited(origin) {
                    return Some(value);
                }

                return ty.visit_origins(visitor);
            }

            Ty::Struct { parameters, .. } => {
                for param in parameters {
                    match param {
                        Parameter::Origin(origin) => {
                            if let ControlFlow::Break(value) = visitor.on_origin_visited(origin) {
                                return Some(value);
                            }
                        }
                        Parameter::Ty(ty) => {
                            return ty.visit_origins(visitor);
                        }
                    }
                }
            }

            Ty::I32 => {}
            Ty::Unit => {}
        }

        None
    }

    // Collects all the origins present in this type, recursively.
    fn collect_origins_into(&self, origins: &mut Vec<Origin>) {
        struct OriginCollector<'a> {
            origins: &'a mut Vec<Origin>,
        }
        impl TyVisitor for OriginCollector<'_> {
            fn on_origin_visited(&mut self, origin: &Name) -> ControlFlow<()> {
                self.origins.push(origin.into());
                ControlFlow::Continue(())
            }
        }
        let mut visitor = OriginCollector { origins };
        self.visit_origins(&mut visitor);
    }
}

impl Place {
    // If the place `base` is deref'd, returns its name without the deref `*`
    fn deref_base(&self) -> Option<&str> {
        if self.base.starts_with('*') {
            Some(&self.base[1..])
        } else {
            None
        }
    }
}

// For readability purposes, and conversion to Soufflé facts, display the facts as the
// textual format.
impl fmt::Display for Facts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Index facts to group them per node
        let mut facts_per_node: BTreeMap<&str, Vec<String>> = BTreeMap::new();

        // Until fact gen is complete, some nodes present in the input program may not
        // have corresponding facts here, so ensure nodes present in CFG edges are
        // created empty.
        //
        // Single statement programs with no facts will still not create empty points though,
        // for that we could use the `ast::Program` as input for this impl.
        //
        // (And we then could add the decls as comments, like the examples currently have)
        //
        for (node1, node2) in &self.cfg_edge {
            facts_per_node.entry(&node1.0).or_default();
            facts_per_node.entry(&node2.0).or_default();
        }

        // Display the facts in the operational order described in the datalog rules.
        for (origin, node) in &self.access_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("access_origin({})", origin.0));
        }

        for (origin, node) in &self.invalidate_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("invalidate_origin({})", origin.0));
        }

        for (origin, node) in &self.clear_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("clear_origin({})", origin.0));
        }

        for (origin1, origin2, node) in &self.introduce_subset {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("introduce_subset({}, {})", origin1.0, origin2.0));
        }

        // Display the indexed data in the frontend format
        for (node_idx, (node, facts)) in facts_per_node.into_iter().enumerate() {
            if node_idx != 0 {
                write!(f, "\n")?;
            }

            // Emit node start, with the statement's `node_text` representation
            let node_text = self
                .node_text
                .iter()
                .find_map(|(node_text, candidate_node)| {
                    if candidate_node.0 == node {
                        Some(node_text.as_ref())
                    } else {
                        None
                    }
                })
                .unwrap_or("(pass)");
            writeln!(f, "{}: {:?} {{", node, node_text)?;

            // Emit all facts first
            for fact in facts {
                writeln!(f, "\t{}", fact)?;
            }

            // And `goto` facts last, with their special syntax. A `goto` is always required,
            // even for the function's exit node (but will have no successors in that case).
            write!(f, "\tgoto")?;
            for (_, succ) in self.cfg_edge.iter().filter(|(from, _)| from.0 == node) {
                write!(f, " {}", succ.0)?;
            }

            writeln!(f, "\n}}")?;
        }

        Ok(())
    }
}
