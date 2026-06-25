//! Structural-search target. The word unwrap appears in comments here but is not
//! a call site, so a textual grep for "unwrap" over-matches. An AST query for
//! method calls must return only the three real `.unwrap()` calls (lines 7, 12,
//! 18), never the comment mentions.
pub fn first() -> i32 {
    let v: Option<i32> = Some(1);
    v.unwrap()
}

pub fn second() -> i32 {
    let r: Result<i32, ()> = Ok(2);
    r.unwrap()
}

pub fn third() -> i32 {
    // another unwrap mention in a comment, also not a call
    let x: Option<i32> = Some(3);
    x.unwrap()
}
