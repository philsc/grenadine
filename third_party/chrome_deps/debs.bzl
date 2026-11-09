"""Repository rule unpacking the pinned Ubuntu .debs in packages.txt."""

def _chrome_deps_impl(rctx):
    entries = []
    for line in rctx.read(rctx.attr.packages).split("\n"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        sha256, url = [field for field in line.split(" ") if field]
        entries.append((sha256, url))

    for i, (sha256, url) in enumerate(entries):
        tmp = "deb{}".format(i)
        rctx.download_and_extract(url = url, sha256 = sha256, type = "deb", output = tmp)
        for member in rctx.path(tmp).readdir():
            if member.basename.startswith("data.tar"):
                rctx.extract(archive = member, output = ".")
        rctx.delete(tmp)

    # Jammy debs still install under /lib; merged-usr systems symlink it to
    # /usr/lib, so fold the two together for a single library directory.
    if rctx.path("lib").exists:
        result = rctx.execute(["bash", "-c", "cp -r lib/. usr/lib/ && rm -rf lib"])
        if result.return_code != 0:
            fail("merging lib/ into usr/lib/ failed: " + result.stderr)

    # Marks the library directory so a test can locate it via $(rootpath).
    rctx.file("usr/lib/x86_64-linux-gnu/.lib_dir", "")

    rctx.file("BUILD.bazel", """
filegroup(
    name = "libs",
    srcs = glob([
        "usr/lib/x86_64-linux-gnu/**",
        "usr/share/fonts/**",
    ]),
    visibility = ["//visibility:public"],
)

alias(
    name = "lib_dir",
    actual = ":usr/lib/x86_64-linux-gnu/.lib_dir",
    visibility = ["//visibility:public"],
)

exports_files(["usr/lib/x86_64-linux-gnu/.lib_dir"])
""")

    # The .debs are pinned by checksum, so the repo can be shared through
    # Bazel's (remote) repo contents cache.
    return rctx.repo_metadata(reproducible = True)

chrome_deps = repository_rule(
    implementation = _chrome_deps_impl,
    attrs = {
        "packages": attr.label(mandatory = True),
    },
)
