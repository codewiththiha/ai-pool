# AGENTS.md

Rules for anyone changing this repository, human or agent. Read it before the
first edit; it is short on purpose.

## Project

`ai-pool` is a client for OpenAI-compatible chat APIs that pools API keys.
One crate, `crates/ai-pool`, in three layers that only talk downward:

| Path | Layer |
|---|---|
| `src/client.rs` | `AiClient` and its builder: wiring only |
| `src/sdk/` | `ChatBuilder`, response formats, strict-schema rewriting |
| `src/http/` | `Dispatcher` (retry table), wire models, SSE parsing |
| `src/engine/` | Key selection: rotation, health, quota gates |
| `src/quota.rs` | Minute and hour windows, concurrency caps |
| `src/storage/` | `KeyStore` trait, `Vault`, `KeyManager`, memory and SQLite stores |
| `src/crypto/` | AES-256-GCM envelope, master key providers |
| `tests/support/` | An in-process HTTP mock server |

The crate knows nothing about any app's prompts, settings or other AI
backends; keep it single-purpose. Provider quirks belong in data (URLs,
headers, body markers), not in provider-specific code paths.

## Behaviour that callers rely on

- The retry table in `README.md` is a contract. Changing a row needs a test
  in `tests/dispatcher_semantics.rs` and a README update in the same commit.
- A key is banned only when waiting cannot fix it; anything time-bound is a
  cooldown.
- Nothing returned by `KeyManager::list_keys` or `Debug` may contain a
  plaintext key.
- A lost or wrong master key never crashes and never deletes rows: keys are
  banned with a reason, and re-adding a key heals its row.
- Stored data stays readable by the next version: a schema change ships a
  migration and a test that opens the old layout.

## Never crash

Responses, headers, stored rows and schemas are untrusted.

- No `unwrap`, `expect`, `panic!` or indexing in library code. Tests may.
- No `unsafe`; the crate forbids it.
- Arithmetic on counts and durations is checked or saturating.
- Poisoned locks are recovered with `PoisonError::into_inner`.
- Blocking work (SQLite, the OS keychain) runs on `spawn_blocking`.
- Recursion over input (schemas) is depth-bounded.

## Comments

A comment carries a constraint the code cannot show. It never narrates the
change, argues for a design, restates the next line or records history; those
go in the commit message or `README.md`.

`python3 tools/comment_check.py FILE...` measures it: three lines, eighty
characters a line, fifteen words a block, doc comments included. Run it on
the files you touched; CI runs it on every tracked file. Usage examples
belong in `README.md`, which is also the crate documentation. A block that
truly needs more says `comment-check: allow` on the line above it.

## Commits

- Conventional commits: `type(scope): summary`, imperative, lower case, no
  trailing period, subject at most 50 characters. Types: `feat`, `fix`, `perf`,
  `refactor`, `docs`, `test`, `build`, `ci`, `chore`. Scopes: `client`, `sdk`,
  `http`, `engine`, `quota`, `storage`, `crypto`, `tools`.
- A body only when the why isn't obvious: a few short lines, wrapped at 72.
  No test counts, file lists or progress reports.
- `python3 tools/commit_check.py --install` arms a hook that refuses long
  subjects; `--range main..HEAD` audits a branch.
- Every commit builds and passes tests alone.
- Never commit credentials, real or test-looking.

## Gates

CI runs all of these on every push; run what you can before pushing:

```text
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo clippy --workspace --no-default-features --all-targets -- -D warnings
cargo test --workspace --all-features
python3 tools/comment_check.py
python3 tools/commit_check.py --range HEAD
```

Never weaken an assertion or edit `.github/workflows/` to turn a check green.

## Definition of done

1. A test fails without the change and passes with it.
2. Every feature combination still builds without warnings.
3. CI is green for the pushed commit.
4. `README.md` describes the new behaviour.
