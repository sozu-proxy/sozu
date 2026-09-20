// Sibling of `mod/LIFECYCLE.md` and the file its bare `h2.rs:N` citations must
// bind to. The digit in the name is the whole point: the obvious character
// class `[A-Za-z_/.-]+` contains none, so a resolver built on it would not see
// a single citation below -- and would report a clean run while doing it.
pub struct Connection;

impl Connection {
    pub fn readable(&self) -> bool {
        true
    }
}
