# Reference fixture — the markdown TARGET half of the guarded surface

A markdown document is a citation TARGET as well as a citing document. Before
sozu-proxy/sozu#1444 the extractor parsed a markdown line reference and then
dropped it, so no markdown-target citation in the tree resolved to anything.

| Parameter | Default | Protects against                   |
| --------- | ------- | ---------------------------------- |
| `knob`    | 20      | the shape a real catalogue row has |

The blank line above is line 10, which is what a markdown-target citation
landing on a blank line must be reported for.
