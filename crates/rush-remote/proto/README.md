# Remote Execution API wire schema

Generate tonic/prost types from these unmodified upstream protocol definitions.
Keep their copyright and Apache-2.0 notices. LICENSE-remote-apis and
LICENSE-googleapis contain the upstream licenses. No backend implementation
source is vendored here.

The build/bazel files come from
[bazelbuild/remote-apis c2c07e3d](https://github.com/bazelbuild/remote-apis/tree/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3).
The google files come from
[googleapis/googleapis bf54c103](https://github.com/googleapis/googleapis/tree/bf54c1031efddde9cdfee0f89254315d0116a5f9).
SOURCES.json records both full revisions. Well-known google/protobuf types
come from the pinned protoc toolchain and prost-types dependency.

Update the upstream pins, imported dependency closure and license notices
together. Rebuild and test the clients against an independent backend after
changing a wire definition. Generating the full schema supplies no automatic
implementation of every protocol feature; read the remote-execution guide
for the supported action and output contracts.
