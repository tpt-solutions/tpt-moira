fn max(a: i32, b: i32) -> i32 {
    if a > b { a } else { b }
}

fn main() {
    let mut s = 0;
    let mut i = 0;
    while i < 10 {
        s = s + i;
        i = i + 1;
    }
    let m = max(s, i);
    let _ = m;
}
