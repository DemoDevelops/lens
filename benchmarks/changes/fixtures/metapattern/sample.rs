// v.unwrap() mentioned in a comment, not a real call site.
fn real_matches(v: Option<i32>, r: Result<i32, i32>) -> bool {
    let a = v.unwrap();
    let b = r.expect("not .unwrap(), a decoy method call");
    let s = "call .unwrap() right here, a string decoy";
    let _ = s.len();
    a > 0 && !b.is_negative()
}

fn compare(x: i32, y: i32) -> bool {
    let same = x == x;
    let diff = x == y;
    let not_same = x != x;
    same && diff && not_same
}
