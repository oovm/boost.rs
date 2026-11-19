# `science-relation-graph`

Relation semantics on science-graph (hash-cons, judgment edges)

Relation semantics on [`science_graph`].

- Bottom layer: ordinary discrete adjacency via [`GraphBuilder`] with [`NodePayload`] /
  [`EdgePayload`] (not CAS M-Graph). Indexed scans use science `out_edges` / `in_edges`.
- Relation layer: hash-cons, label atoms, structural vs judgment edges, EQ paths.
- Staging keeps an unfinished builder. Seal with [`Graph::seal`] / [`Graph::seal_on_heap`].
- Core contract stubs: [`admission`], [`connection`], [`derivation`], [`evidence`], [`transform`], [`conflict`], [`rules`], [`eval`].
