# `science-relation-vm`

Relation register interpreter: frames, fuel, safepoint, and execution lifecycle

Register interpreter for Relation bytecode.

Owns execution lifecycle: frames, fuel, cancel, safepoint hooks, and GC root
orchestration contracts. Does **not** mint admitted tokens, mutate sealed
worlds, or depend on JIT. Kernel interaction goes through [`KernelView`].
