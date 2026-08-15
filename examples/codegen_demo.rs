struct Point {
    x: i32,
    y: i32,
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn max(a: i32, b: i32) -> i32 {
    if a > b { a } else { b }
}

fn sum_to(n: i32) -> i32 {
    let mut total = 0;
    for i in 0..n {
        total = total + i;
    }
    total
}

fn main() {
    let p = Point { x: 3, y: 4 };
    let s = add(p.x, p.y);
    let m = max(s, 10);
    let t = sum_to(5);
    print_i32(s);
    print_i32(m);
    print_i32(t);
}
