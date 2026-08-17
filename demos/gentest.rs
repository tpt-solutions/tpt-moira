struct Wrap<T> { v: T }
impl<T> Clone for Wrap<T> where T: Copy {}
fn main() {}
