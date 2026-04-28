# Contributing to tak-rs

## First-time setup

```sh
git clone git@github.com:copyleftdev/tak-rs.git
cd tak-rs
./scripts/install-hooks.sh
```

That installs the git hooks AND offers to `cargo install` any missing
dev tools (`cargo-deny`, `cargo-nextest`, `cargo-machete`). Run
`scripts/install-deps.sh --yes` directly if you want a non-interactive
install (e.g. in a fresh dev container).

That's it. The pre-commit hook keeps the tree green; the pre-push hook
runs the full gauntlet before anything hits remote.

## How we work

- **No GitHub Actions.** Quality gates run locally via `.githooks/`.
  CI in the cloud would just duplicate what the hooks already enforce.
- **Read `CLAUDE.md` first.** It encodes the locked architectural decisions,
  the persona routing table, and the hot-path invariants. If you're doing
  non-trivial work, invoke the named persona Skill.
- **Read `docs/invariants.md`.** Hot-path invariants (no alloc in dispatch,
  Bytes-clone for fan-out, etc.) are non-negotiable. Either you uphold them
  or you write a PR explaining why the rule should change.

## Branch + commit flow

```sh
git checkout -b <area>/<short-description>
# work
git add <files>
git commit -m "<scope>: <imperative summary>"   # pre-commit runs here
git push -u origin HEAD                          # pre-push runs here
gh pr create --fill
```

Commit messages: imperative ("add X" not "added X"), scope prefix where
useful (`tak-cot:`, `tak-bus:`, `docs:`). Body explains *why*, not *what*.

## When pre-commit fails

Don't `--no-verify`. The hook caught something the compiler didn't:

- `rustfmt`: run `cargo fmt` and re-stage.
- `clippy`: read the message; the lint rationale is in `docs/invariants.md`.
- `cargo-deny`: a dep was added that's banned (chrono/openssl/log/etc.) or a
  CVE landed in the advisory DB. Don't paper over it.
- `nextest`: a test broke. Fix it or revert.

If you genuinely need to bypass (you're recovering from a botched commit, or
the hook itself is broken), `--no-verify` exists. Don't make it a habit.

## Slash commands (Claude Code)

The repo ships project-local agents and slash commands in `.claude/`:

- `/proto-sync` — refresh vendored `.proto` files from upstream Java tree.
- `/bench-hot` — run the firehose criterion bench, diff vs baseline.
- `/fuzz-codec` — `cargo-fuzz` round on `tak-cot` decoders.
- `/check-invariants` — full gauntlet (clippy + deny + machete + dhat + loom).
- `/replay-pcap <file>` — replay a captured TAK pcap through `tak-server`.

Agents (auto-invoked on relevant edits):

- `cot-codec-reviewer` — gates `tak-cot` changes (framing + lossless XML).
- `hot-path-perf` — gates `tak-bus` changes (alloc-free dispatch).
- `unsafe-auditor` — required for any `unsafe` block.
- `proto-vendor` — vendors upstream `.proto` files.
- `bench-baseline` — runs criterion + maintains the perf baseline.

## License

By contributing, you agree your contributions will be dual-licensed
under MIT and Apache-2.0 (see `LICENSE-MIT` and `LICENSE-APACHE`).
