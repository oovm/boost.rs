# `commander-types`

Pure contracts for the `commander.*` CLI interaction family: the frozen process exit codes, stable framework error ids, interactive declaration specs, action-required descriptors, secret string handles, and the machine-readable result envelope builders.

This crate owns no parsing, no I/O, and no platform access. Higher layers (`commander-core`, `commander-prompt`, `commander-terminal`) and downstream hosts consume these types as the single shared vocabulary. Secret values are carried only by `SecretString`, which never prints its contents, and `CommanderError` carries only safe typed arguments.
