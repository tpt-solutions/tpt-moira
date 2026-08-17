fn main() {
    let x = 5;
    let r = &x;
    let s = &x;
    let t = *r + *s;
    let _ = t;
}
