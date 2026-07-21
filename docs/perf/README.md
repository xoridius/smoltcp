# Performance evidence

This directory contains immutable, hash-backed measurements. The Markdown
reports explain the method and conclusions; TSV files are the raw paired runs
and binary metadata needed to audit those conclusions.

- `2026-07-18-after.md` records the hardening comparison and constrained-memory
  results.
- `2026-07-21-cleanup.md` records the final cleanup/refactor comparison.
- `*-before.tsv` and `*-after.tsv` name the two measured revisions; they are not
  pending cleanup work.
- `*-confirmation.tsv`, `*-shipping.tsv`, and `*-release-pairs.tsv` preserve
  the confirmation and matched release runs described by their report.
- `*-binaries.tsv` and `*-lto-runs.tsv` preserve binary identity and build-mode
  evidence.

Verify a set from the repository root:

```sh
(cd docs/perf && sha256sum -c 2026-07-18-evidence.sha256)
(cd docs/perf && sha256sum -c 2026-07-21-cleanup-evidence.sha256)
```

On macOS, use `shasum -a 256 -c` in place of `sha256sum -c`.

Do not rewrite old evidence when a harness or implementation changes. Add a
new dated report, raw measurements, and checksum manifest that identify the
source revisions, binaries, toolchain, feature set, commands, host, and
matched-run method.
