## Summary

<!-- What changes and why. One logical change per pull request. -->

## Related RFC / ADR / issue

<!-- e.g. docs/rfcs/0002-storage-engine.md, docs/adr/0001-no-sqlite-core.md, #123 -->

## Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo fmt --all` and `cargo clippy --workspace --all-targets` are clean
- [ ] No real private payloads in fixtures, tests, or documentation (synthetic or scrubbed only)
- [ ] Privacy canary tests pass (required for adapter, capture, storage, export, and sync changes)
- [ ] Docs / RFC updated if the canonical schema, storage format, frame layout, manifest, `.atdb` container, or IPC protocol changed
- [ ] `CANONICAL_SCHEMA_VERSION` or the storage format version bumped if needed; no numeric field id reused
- [ ] Commits are signed off (`git commit -s`, DCO 1.1)

## Notes for reviewers

<!-- Anything a reviewer should look at first, or known limitations. -->
