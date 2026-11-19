# commander-prompt

Interactive prompt engine for the commander family. It implements the
`PromptApi` contract owned by `commander-core` over an injected IO seam:
eligibility-gated `confirm` / `input` / `select` / `password` / `awaitAction`,
stderr-only questions, absolute deadlines, one active prompt per context,
echo-safe password reads, and non-interactive action-required deferral.

`TerminalPromptIo` and `TerminalFactory` wire the engine to a real terminal
through `boost-terminal`; tests drive the same engine with a scripted fake.
The engine never reads real process state, never writes real stdout, and
never terminates the process.
