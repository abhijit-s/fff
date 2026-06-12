# TODO

## Versioned protocol — follow-up increments
Context: the JSON wire protocol ships alongside legacy bincode via dual-read (ADR-002, `crates/fff-ipc/PROTOCOL.md`). These finish the migration.

- [ ] **Migrate `fffctl` to the versioned JSON protocol.** It still speaks legacy bincode (`MasterRequest`); the dual-read engine serves it unchanged for now. Move it onto `fff-ipc`'s JSON envelope so every first-party client speaks one wire format.
- [ ] **Remove the legacy bincode path.** Only after all clients — editor, `fffctl`, `fff-mcp` — have aged onto JSON and no old daemons remain. Drop the bincode dispatch arms, the `read_frame`/sniff dual-read branch, and the "append variants last" ordinal discipline.
