---
description: Pull latest .proto files from the upstream Java TAK Server clone, vendor into crates/tak-proto, regenerate, and run codec round-trip tests.
allowed-tools: Read, Write, Bash, Grep, Glob, Agent
---

Invoke the `proto-vendor` agent to refresh the vendored protobuf schemas.

The agent will:
1. Diff `.scratch/takserver-java/src/takserver-protobuf/src/main/proto/*.proto` against `crates/tak-proto/proto/*.proto`.
2. Report any wire-format-breaking changes (and stop if found).
3. Copy changed files verbatim, update `crates/tak-proto/build.rs` if new files were added.
4. Run `cargo build -p tak-proto` and `cargo test -p tak-cot --test roundtrip`.
5. Update `crates/tak-proto/UPSTREAM.md` with the upstream SHA + date.

Pre-flight: confirm `.scratch/takserver-java/` exists. If not, suggest:

```
cd /home/ops/Project/tak-rs/.scratch && git clone --depth 1 https://github.com/TAK-Product-Center/Server.git takserver-java
```

If `$ARGUMENTS` is `--upstream-tag <tag>`, fetch that tag in `.scratch/takserver-java/` before diffing. Otherwise compare against the working copy of upstream as cloned.
