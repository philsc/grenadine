# Agent instructions

## Use Bazel for everything

Build, test, run, and lint everything in this repo with Bazel. Bazel provides the pinned Rust toolchain, the third-party crates, and the Python interpreter, so its results are the only ones that count.

Never invoke these tools directly, even if they are installed on the host:

- `cargo`, in any form (`build`, `test`, `run`, `check`, `clippy`, `fmt`, ...)
- `rustc`, `rustup`, `rustfmt`, `clippy-driver`, `wasm-bindgen`
- `python`, `python3`, `pip`, `uv`

The `Cargo.toml` files exist only so that crate_universe and rust-analyzer see the same dependencies as Bazel. They are not a second build system.

Use plain `bazel` commands. `.bazelrc` already configures what this repo needs. Additional flags are not necessary.

## Commands

| Task             | Command                                                  |
| ---------------- | -------------------------------------------------------- |
| Build everything | `bazel build //...`                                      |
| Run all tests    | `bazel test //...`                                       |
| Run one test     | `bazel test //grenadine/core:core_test`                  |
| Run the server   | `bazel run //grenadine/server -- --repo=PATH[:REMOTE]`   |
| Validate default inboxes | `bazel run //grenadine/server:validate_inboxes -- --repo=OWNER/NAME [--repo=...]` |
| Rebuild the test repo | `bazel run //grenadine/testing:create_test_repo -- --other-user=NAME --clone-dir=PATH [--recreate]` |
| Lint             | `bazel build --config=clippy //...`                      |
| Check formatting | `bazel build --config=rustfmt //...`                     |

The formatting check only reports problems. Report them instead of reformatting files, since the user formats code themselves.

## Dependencies

Rust crates: edit the relevant `Cargo.toml`, then run:

```sh
bazel run @rules_rust//tools/upstream_wrapper:cargo -- update --workspace --manifest-path=$PWD/Cargo.toml
CARGO_BAZEL_REPIN=1 bazel mod tidy
```

This is the only permitted use of cargo, and only through the Bazel wrapper above.

Python packages: edit `requirements.in`, then run `bazel run //:requirements`. `bazel test //:requirements_test` checks that the lock is up to date.
