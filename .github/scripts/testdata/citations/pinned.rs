// Fixture source for the pinned-snippet rule. Not compiled: it lives under
// `testdata/`, which the checker skips when it walks the real tree. The two
// documents beside it pin the SAME lines -- one quoting them verbatim, one
// quoting them as they stood before a rename -- so the citation resolves in
// both and only a literal comparison separates the two.
pub struct Pinned {
    inner: u8,
}

impl Pinned {
    pub fn shrink(&mut self) {
        if self.inner > 16 {
            self.inner = 4;
        }
    }
}
