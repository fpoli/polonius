// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::output::Output;

use datafrog::{Iteration, Relation};
use facts::{AllFacts, Atom};

pub(super) fn compute<Region: Atom, Loan: Atom, Point: Atom>(
    dump_enabled: bool,
    mut all_facts: AllFacts<Region, Loan, Point>,
) -> Output<Region, Loan, Point> {
    // Declare that each universal region is live at every point.
    let all_points: BTreeSet<Point> = all_facts
        .cfg_edge
        .iter()
        .map(|&(p, _)| p)
        .chain(all_facts.cfg_edge.iter().map(|&(_, q)| q))
        .collect();

    for &r in &all_facts.universal_region {
        for &p in &all_points {
            all_facts.region_live_at.push((r, p));
        }
    }

    let timer = Instant::now();

    let mut result = Output::new(dump_enabled);

    let errors = {
        // Create a new iteration context, ...
        let mut iteration = Iteration::new();

        // static inputs
        let cfg_edge = iteration.variable::<(Point, Point)>("cfg_edge");
        let killed = all_facts.killed.into();

        // `invalidates` facts, stored ready for joins
        let invalidates = iteration.variable::<((Loan, Point), ())>("invalidates");

        // we need `region_live_at` in both variable and relation forms.
        // (respectively, for join and antijoin).
        let region_live_at_rel =
            Relation::from(all_facts.region_live_at.iter().map(|&(r, p)| (r, p)));
        let region_live_at_var = iteration.variable::<((Region, Point), ())>("region_live_at");

        // `borrow_region` input but organized for join
        let borrow_region_rp = iteration.variable::<((Region, Point), Loan)>("borrow_region_rp");

        // variables, indices for the computation rules, and temporaries for the multi-way joins
        let subset = iteration.variable::<(Region, Region, Point)>("subset");
        let subset_1 = iteration.variable_indistinct("subset_1");
        let subset_2 = iteration.variable_indistinct("subset_2");
        let subset_r1p = iteration.variable_indistinct("subset_r1p");
        let subset_p = iteration.variable_indistinct("subset_p");

        let requires = iteration.variable::<(Region, Loan, Point)>("requires");
        let requires_1 = iteration.variable_indistinct("requires_1");
        let requires_2 = iteration.variable_indistinct("requires_2");
        let requires_bp = iteration.variable_indistinct("requires_bp");
        let requires_rp = iteration.variable_indistinct("requires_rp");

        let borrow_live_at = iteration.variable::<((Loan, Point), ())>("borrow_live_at");

        let live_to_dying_regions_r2pq =
            iteration.variable::<((Region, Point, Point), Region)>("live_to_dying_regions_r2pq");
        let live_to_dying_regions_1 = iteration.variable_indistinct("live_to_dying_regions_1");
        let live_to_dying_regions_2 = iteration.variable_indistinct("live_to_dying_regions_2");

        let dying_region_requires =
            iteration.variable::<((Region, Point, Point), Loan)>("dying_region_requires");
        let dying_region_requires_1 = iteration.variable_indistinct("dying_region_requires_1");
        let dying_region_requires_2 = iteration.variable_indistinct("dying_region_requires_2");

        let dying_can_reach_origins =
            iteration.variable::<((Region, Point), Point)>("dying_can_reach_origins");
        let dying_can_reach_r2q = iteration.variable::<((Region, Point), (Region, Point))>("dying_can_reach");
        let dying_can_reach_1 = iteration.variable_indistinct("dying_can_reach_1");

        let dying_can_reach_live =
            iteration.variable::<((Region, Point, Point), Region)>("dying_can_reach_live");

        let dead_borrow_region_can_reach_root =
            iteration.variable::<((Region, Point), Loan)>("dead_borrow_region_can_reach_root");
        let dead_borrow_region_can_reach_dead =
            iteration.variable::<((Region, Point), Loan)>("dead_borrow_region_can_reach_dead");
        let dead_borrow_region_can_reach_dead_1 =
            iteration.variable_indistinct("dead_borrow_region_can_reach_dead_1");

        // output
        let errors = iteration.variable("errors");

        // load initial facts.
        cfg_edge.insert(all_facts.cfg_edge.into());
        borrow_region_rp.insert(Relation::from(
            all_facts.borrow_region.iter()
                .map(|&(r, b, p)| ((r, p), b))));
        invalidates.insert(Relation::from(
            all_facts.invalidates.iter().map(|&(p, b)| ((b, p), ())),
        ));
        region_live_at_var.insert(Relation::from(
            all_facts.region_live_at.iter().map(|&(r, p)| ((r, p), ())),
        ));
        subset.insert(all_facts.outlives.into());
        requires.insert(all_facts.borrow_region.into());

        // .. and then start iterating rules!
        while iteration.changed() {
            // Cleanup step: remove symmetries
            // - remove regions which are `subset`s of themselves
            //
            // FIXME: investigate whether is there a better way to do that without complicating
            // the rules too much, because it would also require temporary variables and
            // impact performance. Until then, the big reduction in tuples improves performance
            // a lot, even if we're potentially adding a small number of tuples
            // per round just to remove them in the next round.
            subset
                .recent
                .borrow_mut()
                .elements
                .retain(|&(r1, r2, _)| r1 != r2);

            // remap fields to re-index by the different keys
            subset_r1p.from_map(&subset, |&(r1, r2, p)| ((r1, p), r2));
            subset_p.from_map(&subset, |&(r1, r2, p)| (p, (r1, r2)));

            requires_bp.from_map(&requires, |&(r, b, p)| ((b, p), r));
            requires_rp.from_map(&requires, |&(r, b, p)| ((r, p), b));

            // it's now time ... to datafrog:

            // .decl subset(R1, R2, P)
            //
            // At the point P, R1 <= R2.
            //
            // subset(R1, R2, P) :- outlives(R1, R2, P).
            // -> already loaded; outlives is a static input.

            // .decl live_to_dying_regions(R1, R2, P, Q)
            //
            // The regions `R1` and `R2` are "live to dead"
            // on the edge `P -> Q` if:
            //
            // - In P, `R1` <= `R2`
            // - In Q, `R1` is live but `R2` is dead.
            //
            // In that case, `Q` would like to add all the
            // live things reachable from `R2` to `R1`.
            //
            // live_to_dying_regions(R1, R2, P, Q) :-
            //   subset(R1, R2, P),
            //   cfg_edge(P, Q),
            //   region_live_at(R1, Q),
            //   !region_live_at(R2, Q).
            live_to_dying_regions_1
                .from_join(&subset_p, &cfg_edge, |&p, &(r1, r2), &q| ((r1, q), (r2, p)));
            live_to_dying_regions_2.from_join(
                &live_to_dying_regions_1,
                &region_live_at_var,
                |&(r1, q), &(r2, p), &()| ((r2, q), (r1, p)),
            );
            live_to_dying_regions_r2pq.from_antijoin(
                &live_to_dying_regions_2,
                &region_live_at_rel,
                |&(r2, q), &(r1, p)| ((r2, p, q), r1),
            );

            // .decl dying_region_requires((R, P, Q), B)
            //
            // The region `R` requires the borrow `B`, but the
            // region `R` goes dead along the edge `P -> Q`
            //
            // dying_region_requires((R, P, Q), B) :-
            //   requires(R, B, P),
            //   !killed(B, P),
            //   cfg_edge(P, Q),
            //   !region_live_at(R, Q).
            dying_region_requires_1.from_antijoin(&requires_bp, &killed, |&(b, p), &r| (p, (b, r)));
            dying_region_requires_2.from_join(
                &dying_region_requires_1,
                &cfg_edge,
                |&p, &(b, r), &q| ((r, q), (b, p)),
            );
            dying_region_requires.from_antijoin(
                &dying_region_requires_2,
                &region_live_at_rel,
                |&(r, q), &(b, p)| ((r, p, q), b),
            );

            // .decl dying_can_reach_origins(R, P, Q)
            //
            // Contains dead regions where we are interested
            // in computing the transitive closure of things they
            // can reach.
            //
            // dying_can_reach_origins(R2, P, Q) :-
            //   live_to_dying_regions(_, R2, P, Q).
            // dying_can_reach_origins(R, P, Q) :-
            //   dying_region_requires(R, P, Q, _B).
            dying_can_reach_origins.from_map(&live_to_dying_regions_r2pq, |&((r2, p, q), _r1)| ((r2, p), q));
            dying_can_reach_origins.from_map(&dying_region_requires, |&((r, p, q), _b)| ((r, p), q));

            // .decl dying_can_reach(R1, R2, P, Q)
            //
            // Indicates that the region `R1`, which is dead
            // in `Q`, can reach the region `R2` in P.
            //
            // This is effectively the transitive subset
            // relation, but we try to limit it to regions
            // that are dying on the edge P -> Q.
            //
            // dying_can_reach(R1, R2, P, Q) :-
            //   dying_can_reach_origins(R1, P, Q),
            //   subset(R1, R2, P).
            dying_can_reach_r2q.from_join(&dying_can_reach_origins, &subset_r1p, |&(r1, p), &q, &r2| {
                ((r2, q), (r1, p))
            });

            // dying_can_reach(R1, R3, P, Q) :-
            //   dying_can_reach(R1, R2, P, Q),
            //   !region_live_at(R2, Q),
            //   subset(R2, R3, P).
            //
            // This is the "transitive closure" rule, but
            // note that we only apply it with the
            // "intermediate" region R2 is dead at Q.
            dying_can_reach_1.from_antijoin(
                &dying_can_reach_r2q,
                &region_live_at_rel,
                |&(r2, q), &(r1, p)| ((r2, p), (r1, q)),
            );
            dying_can_reach_r2q.from_join(
                &dying_can_reach_1,
                &subset_r1p,
                |&(_r2, p), &(r1, q), &r3| ((r3, q), (r1, p)),
            );

            // .decl dying_can_reach_live(R1, R2, P, Q)
            //
            // Indicates that, along the edge `P -> Q`, the
            // dead (in Q) region R1 can reach the live (in Q)
            // region R2 via a subset relation. This is a
            // subset of the full `dying_can_reach` relation
            // where we filter down to those cases where R2 is
            // live in Q.
            //
            // dying_can_reach_live(R1, R2, P, Q) :-
            //    dying_can_reach(R1, R2, P, Q),
            //    region_live_at(R2, Q).
            dying_can_reach_live.from_join(
                &dying_can_reach_r2q,
                &region_live_at_var,
                |&(r2, q), &(r1, p), &()| ((r1, p, q), r2),
            );

            // subset(R1, R2, Q) :-
            //   subset(R1, R2, P),
            //   cfg_edge(P, Q),
            //   region_live_at(R1, Q),
            //   region_live_at(R2, Q).
            //
            // Carry `R1 <= R2` from P into Q if both `R1` and
            // `R2` are live in Q.
            subset_1.from_join(&subset_p, &cfg_edge, |&_p, &(r1, r2), &q| ((r1, q), r2));
            subset_2.from_join(&subset_1, &region_live_at_var, |&(r1, q), &r2, &()| {
                ((r2, q), r1)
            });
            subset.from_join(&subset_2, &region_live_at_var, |&(r2, q), &r1, &()| {
                (r1, r2, q)
            });

            // subset(R1, R3, Q) :-
            //   live_to_dying_regions(R1, R2, P, Q),
            //   dying_can_reach_live(R2, R3, P, Q).
            subset.from_join(
                &live_to_dying_regions_r2pq,
                &dying_can_reach_live,
                |&(_r2, _p, q), &r1, &r3| (r1, r3, q),
            );

            // .decl requires(R, B, P) -- at the point, things with region R
            // may depend on data from borrow B
            //
            // requires(R, B, P) :- borrow_region(R, B, P).
            // -> already loaded; borrow_region is a static input.

            // requires(R2, B, Q) :-
            //   dying_region_requires(R1, B, P, Q),
            //   dying_can_reach_live(R1, R2, P, Q).
            //
            // Communicate a `R1 requires B` relation across
            // an edge `P -> Q` where `R1` is dead in Q; in
            // that case, for each region `R2` live in `Q`
            // where `R1 <= R2` in P, we add `R2 requires B`
            // to `Q`.
            requires.from_join(
                &dying_region_requires,
                &dying_can_reach_live,
                |&(_r1, _p, q), &b, &r2| (r2, b, q),
            );

            // requires(R, B, Q) :-
            //   requires(R, B, P),
            //   !killed(B, P),
            //   cfg_edge(P, Q),
            //   region_live_at(R, Q).
            requires_1.from_antijoin(&requires_bp, &killed, |&(b, p), &r| (p, (r, b)));
            requires_2.from_join(&requires_1, &cfg_edge, |&_p, &(r, b), &q| ((r, q), b));
            requires.from_join(&requires_2, &region_live_at_var, |&(r, q), &b, &()| {
                (r, b, q)
            });

            // dead_borrow_region_can_reach_root((R, P), B) :-
            //   borrow_region(R, B, P),
            //   !region_live_at(R, P).
            dead_borrow_region_can_reach_root.from_antijoin(
                &borrow_region_rp,
                &region_live_at_rel,
                |&(r, p), &b| ((r, p), b),
            );

            // dead_borrow_region_can_reach_dead((R, P), B) :-
            //   dead_borrow_region_can_reach_root((R, P), B).
            dead_borrow_region_can_reach_dead.from_map(
                &dead_borrow_region_can_reach_root,
                |&tuple| tuple,
            );

            // dead_borrow_region_can_reach_dead((R2, P), B) :-
            //   dead_borrow_region_can_reach_dead(R1, B, P),
            //   subset(R1, R2, P),
            //   !region_live_at(R2, P).
            dead_borrow_region_can_reach_dead_1.from_join(
                &dead_borrow_region_can_reach_dead,
                &subset_r1p,
                |&(_r1, p), &b, &r2| ((r2, p), b),
            );
            dead_borrow_region_can_reach_dead.from_antijoin(
                &dead_borrow_region_can_reach_dead_1,
                &region_live_at_rel,
                |&(r2, p), &b| ((r2, p), b),
            );

            // .decl borrow_live_at(B, P) -- true if the restrictions of the borrow B
            // need to be enforced at the point P
            //
            // borrow_live_at(B, P) :- requires(R, B, P), region_live_at(R, P)
            borrow_live_at.from_join(&requires_rp, &region_live_at_var, |&(_r, p), &b, &()| {
                ((b, p), ())
            });

            // borrow_live_at(B, P) :-
            //   dead_borrow_region_can_reach_dead(R1, B, P),
            //   subset(R1, R2, P),
            //   region_live_at(R2, P).
            //
            // NB: the datafrog code below uses
            // `dead_borrow_region_can_reach_dead_1`, which is equal
            // to `dead_borrow_region_can_reach_dead` and `subset`
            // joined together.
            borrow_live_at.from_join(
                &dead_borrow_region_can_reach_dead_1,
                &region_live_at_var,
                |&(_r2, p), &b, &()| ((b, p), ()),
            );

            // .decl errors(B, P) :- invalidates(B, P), borrow_live_at(B, P).
            errors.from_join(&invalidates, &borrow_live_at, |&(b, p), &(), &()| (b, p));
        }

        if dump_enabled {
            for (region, location) in &region_live_at_rel.elements {
                result
                    .region_live_at
                    .entry(*location)
                    .or_insert(vec![])
                    .push(*region);
            }

            let subset = subset.complete();
            assert!(
                subset.iter().filter(|&(r1, r2, _)| r1 == r2).count() == 0,
                "unwanted subset symmetries"
            );
            for (r1, r2, location) in &subset.elements {
                result
                    .subset
                    .entry(*location)
                    .or_insert(BTreeMap::new())
                    .entry(*r1)
                    .or_insert(BTreeSet::new())
                    .insert(*r2);
            }

            let requires = requires.complete();
            for (region, borrow, location) in &requires.elements {
                result
                    .restricts
                    .entry(*location)
                    .or_insert(BTreeMap::new())
                    .entry(*region)
                    .or_insert(BTreeSet::new())
                    .insert(*borrow);
            }

            let borrow_live_at = borrow_live_at.complete();
            for ((borrow, location), ()) in &borrow_live_at.elements {
                result
                    .borrow_live_at
                    .entry(*location)
                    .or_insert(Vec::new())
                    .push(*borrow);
            }
        }

        errors.complete()
    };

    if dump_enabled {
        println!(
            "errors is complete: {} tuples, {:?}",
            errors.len(),
            timer.elapsed()
        );
    }

    for (borrow, location) in &errors.elements {
        result
            .errors
            .entry(*location)
            .or_insert(Vec::new())
            .push(*borrow);
    }

    result
}
