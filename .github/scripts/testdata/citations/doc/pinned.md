# Pinned-snippet fixture — the quote here matches its source literally

A fenced block may name the exact lines it quotes, in its info string after the
language. The checker then reads those lines and compares them to the block, so
the quote cannot rot behind a line number that still resolves.

```rust pinned.rs:11-15
    pub fn shrink(&mut self) {
        if self.inner > 16 {
            self.inner = 4;
        }
    }
```

A block that names no lines is not compared at all. That is what keeps an
abbreviated or illustrative snippet out of the rule's way instead of forcing it
to be silenced:

```rust
fn shrink(&mut self)
```
