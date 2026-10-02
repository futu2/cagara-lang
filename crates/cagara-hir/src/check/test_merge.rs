//! Tests for the `merge` row operation (Milestone 4)

use super::*;

#[test]
fn merge_basic() {
    // merge {id:int, name:string} {id:string, age:int}
    // = {id:string, name:string, age:int}
    let mut c = Checker::new();
    let left = c.row(vec![("id", c.i), ("name", c.s)]);
    let right = c.row(vec![("id", c.s), ("age", c.i)]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("id", c.s), ("name", c.s), ("age", c.i)]);
    c.unify(&out, &expected).unwrap();
}

#[test]
fn merge_disjoint() {
    // merge {a:int} {b:string} = {a:int, b:string}
    let mut c = Checker::new();
    let left = c.row(vec![("a", c.i)]);
    let right = c.row(vec![("b", c.s)]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("a", c.i), ("b", c.s)]);
    c.unify(&out, &expected).unwrap();
}

#[test]
fn merge_right_wins() {
    // merge {x:int, y:int} {x:string} = {x:string, y:int}
    let mut c = Checker::new();
    let left = c.row(vec![("x", c.i), ("y", c.i)]);
    let right = c.row(vec![("x", c.s)]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("x", c.s), ("y", c.i)]);
    c.unify(&out, &expected).unwrap();
}

#[test]
fn merge_preserves_left_order() {
    // merge {c:int, b:int, a:int} {d:int, b:string}
    // = {c:int, b:string, a:int, d:int}
    let mut c = Checker::new();
    let left = c.row(vec![("c", c.i), ("b", c.i), ("a", c.i)]);
    let right = c.row(vec![("d", c.i), ("b", c.s)]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("c", c.i), ("b", c.s), ("a", c.i), ("d", c.i)]);
    c.unify(&out, &expected).unwrap();
}

#[test]
fn merge_empty_left() {
    // merge  {a:int} = {a:int}
    let mut c = Checker::new();
    let left = c.row(vec![]);
    let right = c.row(vec![("a", c.i)]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("a", c.i)]);
    c.unify(&out, &expected).unwrap();
}

#[test]
fn merge_empty_right() {
    // merge {a:int} {} = {a:int}
    let mut c = Checker::new();
    let left = c.row(vec![("a", c.i)]);
    let right = c.row(vec![]);
    let out = c.merge_rows(&left, &right);

    let expected = c.row(vec![("a", c.i)]);
    c.unify(&out, &expected).unwrap();
}

impl Checker {
    fn row(&mut self, fields: Vec<(&str, Ty)>) -> Ty {
        let fields: Vec<(String, Ty)> = fields
            .into_iter()
            .map(|(k, t)| (k.to_string(), t))
            .collect();
        Ty::Row(fields, Box::new(Ty::Empty))
    }

    fn merge_rows(&mut self, left: &Ty, right: &Ty) -> Ty {
        Ty::Merge(Box::new(left.clone()), Box::new(right.clone()))
    }
}
