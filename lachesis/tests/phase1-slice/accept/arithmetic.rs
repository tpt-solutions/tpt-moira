fn add(a: i32, b: i32) -> i32 { a + b }

fn main() {
    let x = add(2, 3);
    let y = x * 4 - 1;
    let z = (x + y) / 2;
    let _ = z;
}
