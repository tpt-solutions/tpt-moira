async fn add_one(x: i64) -> i64 {
    let y = x + 1;
    y
}

async fn double(x: i64) -> i64 {
    let a = await add_one(x);
    let b = await add_one(a);
    a + b
}

fn main() {
    let r = await double(10);
    print_i64(r);
}
