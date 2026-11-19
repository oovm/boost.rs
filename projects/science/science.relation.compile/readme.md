# `science-relation-compile`

Parse and lower .metis sources (Metis AST)

Lowering / execution entry (Living grammar).

Current foundation: **parse** `.metis` via `data-text-metis`, then declare islands into
[`IslandStore`] via [`lower_module`]. FOL prove and `action` verify are not yet
implemented — do not revive the retired `Island::check` script dialect.
