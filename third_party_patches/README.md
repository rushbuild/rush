# third_party_patches

Committed workarounds for upstream issues that block rush-built TensorFlow
but whose fix belongs upstream (or requires a dep version bump that isn't
in scope for a given session).

Every patch here should eventually go away. The rule:

1. **Nothing lands here to hide a rush bug** — that's CLAUDE.md "NO FUCKING
   STUBS" territory. These are only for real upstream issues we can point
   at.
2. Each patch has a one-line header comment stating the upstream cause
   and how to remove the workaround (typically "upgrade X to version Y").
3. The bootstrap script (`scripts/bootstrap-prune.sh`) applies the patches
   idempotently so the env is reproducible.

## Current patches

- **`tensorflow-absl-strview-workaround.patch`** — Debian 15 ships
  `libabsl-dev` (the `absl::debian7::` inline namespace) which lacks the
  `AlphaNum(std::string_view)` constructor that TF sources assume. Two
  `.cc`/`.h` files use `absl::StrCat(std::string_view, ...)` which then
  fails to match any overload. The patch wraps the string_view args in
  `std::string(...)`. Remove after switching rush to the TF-bundled
  `@com_google_absl` build (which has the newer constructor).
