# Upstream Java loading fixture

Keep `native.bzl` and `defs.bzl` byte-identical to rules_java 5.3.5 commit
`f8786592b079c5e23580a4e91ac865620177d189`. Exercise their real alias exports
and native-rule forwarding through Rush's module loader. Retain the upstream
Apache-2.0 license and headers. `PROVENANCE.json` records the official source
URLs and SHA256 values for both files and the license.

The regression loads the real Java rule definitions. It does not qualify Java
compilation, provider construction or annotation processing; those operations
must retain explicit unsupported errors until their semantics are implemented.
