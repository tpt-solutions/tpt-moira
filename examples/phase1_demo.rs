use std::collections::HashMap;

struct Point {
    x: i32,
    y: i32,
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let mut table: HashMap<i32, i32> = HashMap::new();
    table.insert(1, 10);

    let x = 10;
    let y: i32 = x * 2 + (3 - 1);
    let p = Point { x: 1, y: 2 };
    let _ = p.x + p.y;

    let r = if x > 5 { add(x, y) } else { 0 };
    let mut sum = 0;
    for i in 0..10 {
        sum += i;
    }
    while sum > 0 {
        sum -= 1;
    }
    let _ = (r, table, sum);
}
