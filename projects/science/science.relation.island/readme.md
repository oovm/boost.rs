# `science-relation-island`

Relation island registry and staging handles

Island registry: accepted graphs plus disposable staging arenas.

Accepted islands are frozen for mutation. Staging can be discarded or
promoted into the accepted table. Each island carries a monotonic
[`WorldId::version`] for Core-relative world context.
