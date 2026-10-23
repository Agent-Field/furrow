# Portable exchange fixture

`linux.agef` and `linux-head.txt` were produced by actual Linux capture and
`exchange export`. They contain an executable with mixed LF/CRLF, NUL and 0xff,
a cross-directory hard-link pair, a nested path, Windows-reserved names, case-only siblings and a symlink.
The test uses fixed public test keys (0x11 and 0x22); these are not credentials.

Regenerate on Linux from the repository root:

```sh
FURROW_GENERATE_PORTABLE="$PWD/testdata/windows" cargo test -p furrow-core --test portable --locked -j 4
```

On Windows, set `FURROW_PORTABLE_OUT` to a throwaway directory and run the
`portable-*.exe` test binary (or `cargo test -p furrow-core --test portable`).
Carry the resulting `windows.agef` and `windows-head.txt` back to Linux, then:

```sh
FURROW_PORTABLE_IN=/path/to/windows-output cargo test -p furrow-core --test portable --locked -j 4
```

The M2/W3 test asserts hard-link identity after restore and re-seal, bytes,
slash-separated tree paths, preserved executable mode,
0644 for new files, and a second materialization after a fresh encrypted
export/import. The Windows branch also asserts the individual omissions and
reasons, including a symlink refusal when the token cannot create links.
