"""Linter aspects. Run them with: bazel build --config=lint //..."""

load("@aspect_rules_lint//lint:ruff.bzl", "lint_ruff_aspect")

ruff = lint_ruff_aspect(
    binary = Label("@aspect_rules_lint//lint:ruff_bin"),
    configs = [Label("//:ruff.toml")],
)
