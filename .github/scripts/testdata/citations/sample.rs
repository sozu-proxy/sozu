// Fixture source for the citation resolver self-test. Not compiled: it lives
// under `testdata/`, which the resolver skips when it walks the real tree.
pub struct Sample;


impl Sample {
    pub fn readable(&self) -> bool {
        true
    }
}
