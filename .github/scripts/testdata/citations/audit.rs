// Fixture source for the citation audit self-test. Not compiled: it lives
// under `testdata/`, which the resolver skips when it walks the real tree.
pub struct Ledger {
    entries: Vec<u32>,
    retired: Vec<u32>,
}

impl Ledger {
    pub fn retire_entry(&mut self, id: u32) {
        // Drop the entry before the caller reads the tail again.
        self.entries.retain(|entry| *entry != id);
        self.retired.push(id);
        self.recount_all();
    }

    fn recount_all(&mut self) {
        let live = self.entries.len();
        let gone = self.retired.len();
        let mut seen = 0;
        for entry in &self.entries {
            if *entry > 0 {
                seen += 1;
            }
        }
        for entry in &self.retired {
            if *entry > 0 {
                seen += 1;
            }
        }
        let total = live + gone + seen;
        let _ = total;
    }
}
