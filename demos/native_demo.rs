// A Phase-1 source that stays within the from-scratch native backend's
// restricted subset: integer arithmetic, control flow, recursion, and the
// `print_i32` builtin. (Struct literals/fields are handled by the Stage-0 C
// backend instead.)

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn fib(n: i32) -> i32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn sum_to(n: i32) -> i32 {
    let mut total = 0;
    for i in 0..n {
        total = total + i;
    }
    total
}

fn main() {
    let x = add(3, 4);
    let y = fib(10);
    let z = sum_to(100);
    print_i32(x);
    print_i32(y);
    print_i32(z);
}
