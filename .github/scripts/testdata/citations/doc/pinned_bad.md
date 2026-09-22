# Broken pinned-snippet fixture — the citation resolves and the quote is stale

The span below resolves to a non-blank line at both ends, so the blank-line rule
passes this document, and the span did not move, so the drift rule passes it
too. The quoted text is one rename out of date, and only a literal comparison
against the cited lines reports it.

```rust pinned.rs:11-15
    pub fn shrink_buffers(&mut self) {
        if self.buf > 16 {
            self.buf = 4;
        }
    }
```
