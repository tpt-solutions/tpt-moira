fn sum_to(n: i32) -> i32 {
    let mut s = 0;
    for i in 0..n {
        s = s + i;
    }
    s
}

fn main() {
    let r = sum_to(5);
    let _ = r;
}
