# Nested-example fixture — a document that SHOWS a pin without carrying one

Documentation that explains the annotation has to display an opening fence
without pairing it. CommonMark closes a fenced block only on a fence at least
as long as the one that opened it, carrying nothing after it, so the four-tick
block below holds an unmatched three-tick opener as ordinary text:

````markdown
```rust pinned.rs:11-15
    pub fn shrink(&mut self) {
````

The block after it is a real pin and must still be compared. A scanner that
closed the four-tick block on that inner opener would run out of phase from
here on, stop seeing this pin, and exit 0 over a shrinking count:

```rust pinned.rs:11-15
    pub fn shrink(&mut self) {
        if self.inner > 16 {
            self.inner = 4;
        }
    }
```
