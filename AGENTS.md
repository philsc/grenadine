# Agent instructions

## Use Bazel for everything

Build, test, run, and lint everything in this repo with Bazel. Bazel provides the pinned Rust toolchain, the third-party crates, the Python interpreter, Node, and the formatters, so its results are the only ones that count.

Never invoke these tools directly, even if they are installed on the host:

- `cargo`, in any form (`build`, `test`, `run`, `check`, `clippy`, `fmt`, ...)
- `rustc`, `rustup`, `rustfmt`, `clippy-driver`, `wasm-bindgen`
- `pip`, `uv`
- `node`, `npm`, `npx`, `pnpm`
- `prettier`, `ruff`, `buildifier`, `taplo`

The `Cargo.toml` files exist only so that crate_universe and rust-analyzer see the same dependencies as Bazel. They are not a second build system.

Use plain `bazel` commands. `.bazelrc` already configures what this repo needs. Additional flags are not necessary.

## Commands

| Task             | Command                                                  |
| ---------------- | -------------------------------------------------------- |
| Build everything | `bazel build //...`                                      |
| Run all tests    | `bazel test //...`                                       |
| Run one test     | `bazel test //grenadine/core:core_test`                  |
| Run the UI tests | `bazel test //grenadine/testing:ui_test`               |
| Run the server   | `bazel run //grenadine/server -- --repo=PATH[:REMOTE]`   |
| Validate default inboxes | `bazel run //grenadine/server:validate_inboxes -- --repo=OWNER/NAME [--repo=...]` |
| Rebuild the test repo | `bazel run //grenadine/testing:create_test_repo -- --other-user=NAME --clone-dir=PATH [--recreate]` |
| Lint             | `bazel build --config=lint //...`                        |
| Check formatting | `bazel run //:format.check`                              |
| Format files (user only) | `bazel run //:format [-- PATH...]`               |

`--config=lint` runs clippy on the Rust targets and ruff on the Python targets.

`//:format.check` covers Rust, Python, Starlark, TOML, and, through prettier, HTML, CSS, Markdown, JSON, and GraphQL. It only reports problems. Report them instead of running `//:format`, since the user formats code themselves.

## Dependencies

Rust crates: edit the relevant `Cargo.toml`, then run:

```sh
bazel run @rules_rust//tools/upstream_wrapper:cargo -- update --workspace --manifest-path=$PWD/Cargo.toml
CARGO_BAZEL_REPIN=1 bazel mod tidy
```

This is the only permitted use of cargo, and only through the Bazel wrapper above.

Reformatting a `Cargo.toml` changes the manifest digest in `cargo-bazel-lock.json`, so rerun `CARGO_BAZEL_REPIN=1 bazel mod tidy` afterwards.

Python packages: edit `requirements.in`, then run `bazel run //:requirements`. `bazel test //:requirements_test` checks that the lock is up to date.

npm packages: edit `package.json`, then run:

```sh
bazel run -- @pnpm//:pnpm --dir $PWD install --lockfile-only
```
