struct Point { x: i32, y: i32 }

fn make() -> Point {
    Point { x: 1, y: 2 }
}

fn main() {
    let p = make();
    let z = p.x + p.y;
    let _ = z;
}
