# `commander-core`

Execution layer of the `commander.*` family: command tree declaration, argv parsing with stable usage errors, the per-run `CommandContext`, output routing between human diagnostics and the machine envelope, and the interactive effect gate.

The crate never touches the real process: it does not read environment variables or standard streams, never calls `process::exit`, and never writes standard output. Hosts inject an `Environment` snapshot and `Ports` (clock, writer, localizer, cancel token), and `run` returns a `RunOutcome` carrying the frozen exit code and, in JSON mode, exactly one `wae.cli-result/v0` envelope value.

Prompting itself lives in `commander-prompt`; this crate owns only the eligibility snapshot, the stdin-source claim, and the single active prompt lease that the prompt state machine acquires.
