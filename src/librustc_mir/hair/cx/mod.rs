// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This module contains the code to convert from the wacky tcx data
//! structures into the hair. The `builder` is generally ignorant of
//! the tcx etc, and instead goes through the `Cx` for most of its
//! work.
//!

use hair::*;

use rustc::middle::const_val::{ConstEvalErr, ConstVal};
use rustc_const_eval::ConstContext;
use rustc_data_structures::indexed_vec::Idx;
use rustc::hir::def_id::{DefId, LOCAL_CRATE};
use rustc::hir::map::blocks::FnLikeNode;
use rustc::middle::region;
use rustc::infer::InferCtxt;
use rustc::ty::subst::Subst;
use rustc::ty::{self, Ty, TyCtxt};
use rustc::ty::subst::Substs;
use syntax::ast;
use syntax::symbol::Symbol;
use rustc::hir;
use rustc_const_math::{ConstInt, ConstUsize};
use std::rc::Rc;

#[derive(Clone)]
pub struct Cx<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'gcx, 'tcx>,
    infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,

    pub root_lint_level: ast::NodeId,
    pub param_env: ty::ParamEnv<'gcx>,

    /// Identity `Substs` for use with const-evaluation.
    pub identity_substs: &'gcx Substs<'gcx>,

    pub region_scope_tree: Rc<region::ScopeTree>,
    pub tables: &'a ty::TypeckTables<'gcx>,

    /// This is `Constness::Const` if we are compiling a `static`,
    /// `const`, or the body of a `const fn`.
    constness: hir::Constness,

    /// What kind of body is being compiled.
    pub body_owner_kind: hir::BodyOwnerKind,

    /// True if this constant/function needs overflow checks.
    check_overflow: bool,
}

impl<'a, 'gcx, 'tcx> Cx<'a, 'gcx, 'tcx> {
    pub fn new(infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
               src_id: ast::NodeId) -> Cx<'a, 'gcx, 'tcx> {
        let tcx = infcx.tcx;
        let src_def_id = tcx.hir.local_def_id(src_id);
        let body_owner_kind = tcx.hir.body_owner_kind(src_id);

        let constness = match body_owner_kind {
            hir::BodyOwnerKind::Const |
            hir::BodyOwnerKind::Static(_) => hir::Constness::Const,
            hir::BodyOwnerKind::Fn => {
                let fn_like = FnLikeNode::from_node(infcx.tcx.hir.get(src_id));
                fn_like.map_or(hir::Constness::NotConst, |f| f.constness())
            }
        };

        let attrs = tcx.hir.attrs(src_id);

        // Some functions always have overflow checks enabled,
        // however, they may not get codegen'd, depending on
        // the settings for the crate they are translated in.
        let mut check_overflow = attrs.iter()
            .any(|item| item.check_name("rustc_inherit_overflow_checks"));

        // Respect -C overflow-checks.
        check_overflow |= tcx.sess.overflow_checks();

        // Constants and const fn's always need overflow checks.
        check_overflow |= constness == hir::Constness::Const;

        let lint_level = lint_level_for_hir_id(tcx, src_id);
        Cx {
            tcx,
            infcx,
            root_lint_level: lint_level,
            param_env: tcx.param_env(src_def_id),
            identity_substs: Substs::identity_for_item(tcx.global_tcx(), src_def_id),
            region_scope_tree: tcx.region_scope_tree(src_def_id),
            tables: tcx.typeck_tables_of(src_def_id),
            constness,
            body_owner_kind,
            check_overflow,
        }
    }

}

impl<'a, 'gcx, 'tcx> Cx<'a, 'gcx, 'tcx> {
    /// Normalizes `ast` into the appropriate `mirror` type.
    pub fn mirror<M: Mirror<'tcx>>(&mut self, ast: M) -> M::Output {
        ast.make_mirror(self)
    }

    pub fn usize_ty(&mut self) -> Ty<'tcx> {
        self.tcx.types.usize
    }

    pub fn usize_literal(&mut self, value: u64) -> Literal<'tcx> {
        match ConstUsize::new(value, self.tcx.sess.target.usize_ty) {
            Ok(val) => {
                Literal::Value {
                    value: self.tcx.mk_const(ty::Const {
                        val: ConstVal::Integral(ConstInt::Usize(val)),
                        ty: self.tcx.types.usize
                    })
                }
            }
            Err(_) => bug!("usize literal out of range for target"),
        }
    }

    pub fn bool_ty(&mut self) -> Ty<'tcx> {
        self.tcx.types.bool
    }

    pub fn unit_ty(&mut self) -> Ty<'tcx> {
        self.tcx.mk_nil()
    }

    pub fn true_literal(&mut self) -> Literal<'tcx> {
        Literal::Value {
            value: self.tcx.mk_const(ty::Const {
                val: ConstVal::Bool(true),
                ty: self.tcx.types.bool
            })
        }
    }

    pub fn false_literal(&mut self) -> Literal<'tcx> {
        Literal::Value {
            value: self.tcx.mk_const(ty::Const {
                val: ConstVal::Bool(false),
                ty: self.tcx.types.bool
            })
        }
    }

    pub fn const_eval_literal(&mut self, e: &hir::Expr) -> Literal<'tcx> {
        let tcx = self.tcx.global_tcx();
        let const_cx = ConstContext::new(tcx,
                                         self.param_env.and(self.identity_substs),
                                         self.tables());
        match const_cx.eval(tcx.hir.expect_expr(e.id)) {
            Ok(value) => Literal::Value { value },
            Err(s) => self.fatal_const_eval_err(&s, e.span, "expression")
        }
    }

    pub fn pattern_from_hir(&mut self, p: &hir::Pat) -> Pattern<'tcx> {
        let tcx = self.tcx.global_tcx();
        let p = match tcx.hir.get(p.id) {
            hir::map::NodePat(p) | hir::map::NodeBinding(p) => p,
            node => bug!("pattern became {:?}", node)
        };
        Pattern::from_hir(tcx,
                          self.param_env.and(self.identity_substs),
                          self.tables(),
                          p)
    }

    pub fn fatal_const_eval_err(&mut self,
        err: &ConstEvalErr<'tcx>,
        primary_span: Span,
        primary_kind: &str)
        -> !
    {
        err.report(self.tcx, primary_span, primary_kind);
        self.tcx.sess.abort_if_errors();
        unreachable!()
    }

    pub fn trait_method(&mut self,
                        trait_def_id: DefId,
                        method_name: &str,
                        self_ty: Ty<'tcx>,
                        params: &[Ty<'tcx>])
                        -> (Ty<'tcx>, Literal<'tcx>) {
        let method_name = Symbol::intern(method_name);
        let substs = self.tcx.mk_substs_trait(self_ty, params);
        for item in self.tcx.associated_items(trait_def_id) {
            if item.kind == ty::AssociatedKind::Method && item.name == method_name {
                let method_ty = self.tcx.type_of(item.def_id);
                let method_ty = method_ty.subst(self.tcx, substs);
                return (method_ty,
                        Literal::Value {
                            value: self.tcx.mk_const(ty::Const {
                                val: ConstVal::Function(item.def_id, substs),
                                ty: method_ty
                            }),
                        });
            }
        }

        bug!("found no method `{}` in `{:?}`", method_name, trait_def_id);
    }

    pub fn all_fields(&mut self, adt_def: &ty::AdtDef, variant_index: usize) -> Vec<Field> {
        (0..adt_def.variants[variant_index].fields.len())
            .map(Field::new)
            .collect()
    }

    pub fn needs_drop(&mut self, ty: Ty<'tcx>) -> bool {
        let (ty, param_env) = self.tcx.lift_to_global(&(ty, self.param_env)).unwrap_or_else(|| {
            bug!("MIR: Cx::needs_drop({:?}, {:?}) got \
                  type with inference types/regions",
                 ty, self.param_env);
        });
        ty.needs_drop(self.tcx.global_tcx(), param_env)
    }

    fn lint_level_of(&self, node_id: ast::NodeId) -> LintLevel {
        let hir_id = self.tcx.hir.definitions().node_to_hir_id(node_id);
        let has_lint_level = self.tcx.dep_graph.with_ignore(|| {
            self.tcx.lint_levels(LOCAL_CRATE).lint_level_set(hir_id).is_some()
        });

        if has_lint_level {
            LintLevel::Explicit(node_id)
        } else {
            LintLevel::Inherited
        }
    }

    pub fn tcx(&self) -> TyCtxt<'a, 'gcx, 'tcx> {
        self.tcx
    }

    pub fn tables(&self) -> &'a ty::TypeckTables<'gcx> {
        self.tables
    }

    pub fn check_overflow(&self) -> bool {
        self.check_overflow
    }

    pub fn type_moves_by_default(&self, ty: Ty<'tcx>, span: Span) -> bool {
        self.infcx.type_moves_by_default(self.param_env, ty, span)
    }
}

fn lint_level_for_hir_id(tcx: TyCtxt, mut id: ast::NodeId) -> ast::NodeId {
    // Right now we insert a `with_ignore` node in the dep graph here to
    // ignore the fact that `lint_levels` below depends on the entire crate.
    // For now this'll prevent false positives of recompiling too much when
    // anything changes.
    //
    // Once red/green incremental compilation lands we should be able to
    // remove this because while the crate changes often the lint level map
    // will change rarely.
    tcx.dep_graph.with_ignore(|| {
        let sets = tcx.lint_levels(LOCAL_CRATE);
        loop {
            let hir_id = tcx.hir.definitions().node_to_hir_id(id);
            if sets.lint_level_set(hir_id).is_some() {
                return id
            }
            let next = tcx.hir.get_parent_node(id);
            if next == id {
                bug!("lint traversal reached the root of the crate");
            }
            id = next;
        }
    })
}

mod block;
mod expr;
mod to_ref;
