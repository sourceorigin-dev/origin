use origin_shared::{Record, Tree};

use super::*;

/// The history of `edges`, each a record and its parents.
fn ancestry(edges: &[(u8, &[u8])]) -> Ancestry<u8> {
    let tips: Vec<_> = edges.iter().map(|&(id, _)| id).collect();
    let empty = Tree::default().encode().id().into();
    Ancestry::walk(&tips, |id| {
        let parents = edges.iter().find(|&&(of, _)| of == id).map(|&(_, p)| p);
        Ok::<_, ()>((parents.unwrap_or_default().to_vec(), empty))
    })
    .unwrap()
}

fn bases(history: &Ancestry<u8>, tips: &[u8]) -> Vec<u8> {
    let groups: Vec<_> = tips.iter().map(std::slice::from_ref).collect();
    history.best_common(&groups)
}

#[test]
fn the_newest_shared_ancestor_wins_however_far() {
    // 2 follows 1; 3 to 6 and 7 to 10 grow apart from 2, then each
    // lands a side line forked from 1: 1 is nearer, but 2 is newer.
    let history = ancestry(&[
        (2, &[1]),
        (3, &[2]),
        (4, &[3]),
        (5, &[4]),
        (6, &[5]),
        (7, &[2]),
        (8, &[7]),
        (9, &[8]),
        (10, &[9]),
        (11, &[1]),
        (12, &[6, 11]),
        (13, &[1]),
        (14, &[10, 13]),
    ]);
    assert_eq!(bases(&history, &[12, 14]), [2]);
    assert_eq!(bases(&history, &[12, 6]), [6], "an ancestor of the other");
}

#[test]
fn criss_cross_merges_have_two_bases() {
    // 2 and 3 fork from 1, then each merges the other.
    let history = ancestry(&[(2, &[1]), (3, &[1]), (4, &[2, 3]), (5, &[3, 2])]);
    assert_eq!(bases(&history, &[4, 5]), [2, 3]);
    assert_eq!(bases(&history, &[4, 5, 3]), [3]);
    // Merging those two needs their own base.
    assert_eq!(history.best_common(&[&[2], &[3]]), [1]);
}

#[test]
fn unrelated_histories_have_none() {
    let history = ancestry(&[(2, &[1]), (4, &[3])]);
    assert_eq!(bases(&history, &[2, 4]), Vec::<u8>::new());
}
