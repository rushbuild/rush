# Source-import checks

The initial source import passed these checks during preparation:

- Cargo resolved locked, offline workspace metadata for all 23 crates.
- The ownership checker covered 1,961 staged paths across ten components.
- All three routing tests passed, including overlapping review ownership and
  rejection of escaping paths and incomplete component records.
- The Chores file inventory and SHA-256 pins matched. Its public skill download
  matched the included skill. The source starter had no uncommitted changes.
- The policy passed schema validation and deliberately failed complete-policy
  admission because account, budget, expiry and delegation bindings are absent.
- All 19 local links in the new entry documents and HTML page resolved.
- The complete included file set passed the private-identifier and common
  credential-pattern scan. This scan is a release hygiene check, not a security
  audit or proof that no secret can exist.
- Headless Chrome rendered the HTML at desktop and mobile widths; both renders
  were visually inspected, with the mobile grid sizing corrected.
- The staged whitespace check passed excluding inherited third-party patch
  files, whose context lines intentionally contain spaces.

NOT RUN: Rust compilation, crate suites, full Bazel compatibility, Linux
capture/replay qualification, performance comparisons, unattended provider
execution and authenticated forge writes. No new engine certification or
binary-release claim follows from the source-import checks. The first TODO
requires establishing those applicable gates on the approved runner.

The ownership checker reports repository-relative paths and never executes its
listed test commands. Detailed source/omission hashes, account information and
provider usage receipts remain private with the migration record.
