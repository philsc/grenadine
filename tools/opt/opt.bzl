"""Builds a target's files with --compilation_mode=opt, whatever the build's."""

def _opt_transition_impl(_settings, _attr):
    return {"//command_line_option:compilation_mode": "opt"}

_opt_transition = transition(
    implementation = _opt_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:compilation_mode"],
)

def _opt_files_impl(ctx):
    files = depset(transitive = [src[DefaultInfo].files for src in ctx.attr.srcs])
    return [DefaultInfo(files = files)]

opt_files = rule(
    implementation = _opt_files_impl,
    doc = "Forwards the files of `srcs`, built with --compilation_mode=opt.",
    attrs = {
        "srcs": attr.label_list(cfg = _opt_transition, mandatory = True),
    },
)
