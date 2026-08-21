fn sq(x: i32) -> i32 { x * x }

fn hyp(a: i32, b: i32) -> i32 {
    let a2 = sq(a);
    let b2 = sq(b);
    a2 + b2
}

fn main() {
    let r = hyp(3, 4);
    let _ = r;
}
