# Contributing

## Checks

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs these on every pull request. Run them locally before you push:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check            # needs cargo-deny 0.20 or newer
```

The toolchain is pinned in `rust-toolchain.toml`. `.cargo/config.toml` sets
`LIBSQLITE3_FLAGS`, so the bundled SQLite gets the same build flags locally and in CI.

## Dependency direction

Each crate has one job. Internal dependencies only point down
([docs/plan/phase1.md](docs/plan/phase1.md), chapter 2):

| Crate | May depend on (`[dependencies]`, `[build-dependencies]`) |
| --- | --- |
| `musubi-model` | nothing internal; no I/O |
| `musubi-recurrence` | nothing internal |
| `musubi-store-sqlite` | `musubi-model` |
| `musubi-google` | `musubi-model` |
| `musubi-engine` | `musubi-model`, `musubi-recurrence` |
| `musubi-syncd` | all of the above |
| `musubi-testkit` | `musubi-model` (`publish = false`, used only as a dev-dependency and for its own binaries) |

Rules:

- The engine knows neither SQLite nor Google. It works against the traits in
  `musubi-model`. Its tests may use `musubi-store-sqlite` and `musubi-testkit` as
  dev-dependencies, but never `musubi-google`: the path engine + Google adapter +
  fake Google server is tested in `musubi-syncd`.
- Adapters know neither the engine nor the store.
- Only `musubi-syncd` uses `musubi-store-sqlite` and `musubi-google` directly.
- Any other dev-dependency between workspace crates is allowed.

### How it is checked

`crates/musubi-testkit/tests/dependency_direction.rs` reads `cargo metadata` and
compares every workspace crate's internal dependencies with its `ALLOWED` and
`DEV_FORBIDDEN` tables. It runs in `cargo test --workspace`, so a violation fails
the CI `test` job, for example:

```
dependency direction violated (see CONTRIBUTING.md):
musubi-engine -> musubi-store-sqlite (normal): not allowed
```

A new crate under `crates/` fails the same test until it is added to `ALLOWED`.

cargo-deny does not do this check: its `bans.deny` `wrappers` cannot tell
dev-dependencies apart, and `graph.exclude-dev` would also hide the forbidden
engine → google dev-dependency and drop dev-dependencies from the licence and
advisory checks.

### Changing the rules

Change the table above, chapter 2 of the plan and the `ALLOWED` or
`DEV_FORBIDDEN` table in the test in the same pull request, and say why in its
description.

## External dependencies

- Add a dependency only with a reason in the pull request. Declare its version
  once in `[workspace.dependencies]` of the root `Cargo.toml`; crates use
  `name.workspace = true`.
- Internal path dependencies carry `version = "0.0.0"` next to `path`, because
  `deny.toml` forbids wildcard versions.
- `cargo deny check` must pass. A new licence, advisory exception or duplicate
  version (`bans.skip`) goes into `deny.toml` with a reason; an advisory
  exception also needs a review date.

## Commits and pull requests

- One topic per pull request. Conventional commit subjects (`feat(model): …`,
  `ci: …`, `docs(plan): …`).
- If an interface in `docs/plan/` turns out wrong while you implement it, fix the
  plan text in the same pull request and list the change in its description.
- No `unsafe` (the workspace forbids it), no `todo!()` or `unimplemented!()` in
  merged code.
