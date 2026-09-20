# Citation fixture — every citation here must resolve

`Sample::readable` is at `sample.rs:6`, and its body spans `sample.rs:6-8`.

The struct and the impl block are at `sample.rs:3/6`, and the comma form
`sample.rs:1, 10` resolves both halves.

A continuation may wrap the prose line (`sample.rs:3,
6`) and must still resolve.
