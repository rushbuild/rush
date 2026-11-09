# Execute actions on remote workers

Send declared inputs to a Remote Execution API v2 (REAPI) service, execute on
its selected worker, and restore verified regular-file outputs. Use an existing
REAPI backend, such as [NativeLink](https://github.com/TraceMachina/nativelink).
Rush supplies the client; the backend supplies scheduling, worker isolation,
toolchains, GPUs and persistent content-addressed storage (CAS).

## Build C++ targets remotely

Select a worker whose compiler, linker and system headers match the requested
platform. Assign an immutable `toolchain` property to each toolchain image;
change that property whenever its contents change. The property is an
operator-supplied identity, not an image attestation.

```sh
RUSH_CXX=g++ RUSH_LINKER=bfd rush build //:app \
  --remote-executor http://127.0.0.1:9000 \
  --remote-platform toolchain=my-gcc-image-v1 \
  --remote-platform OSFamily=linux \
  --remote-instance example
```

Replace the endpoint, instance and platform with the backend's configuration.
Omit `--remote-instance` for an empty instance. Use HTTPS with a trusted server
certificate or an authenticated tunnel to a loopback listener. Keep private
addresses and credentials outside build files. Rush loads native TLS trust
roots; this interface supplies no bearer-token metadata or client certificate.

The native path supports the tested `cc_library` and `cc_binary` compile,
archive and link actions, including thin-archive members confined to the
workspace. It executes through the action manager in-process and uses a fresh
local action-cache view. Remote platform and endpoint values participate in
analysis reuse identity. It never substitutes local execution when a worker
fails. Native scheduler dispatch and other rule types return an error; use an
explicit action manifest for those commands. Keep action working directories
at the workspace root. Unsupported embedded absolute paths return an error.

Keep the client toolchain discovery compatible with the worker. The example
pins `bfd` to avoid choosing a locally available fast linker absent on the
worker. Supply `g++`, `ar`, their subprocesses and required system headers in
the worker image. The backend must provide a usable default `PATH` too.
`--disk-cache=false` controls Rush's local cache; it does not disable the
remote action cache.

## Execute an explicit file action

Write `action.json` with an argument vector, complete action environment,
relative inputs, required outputs and worker properties:

```json
{
  "arguments": ["/usr/bin/g++", "-O2", "hello.cc", "-o", "hello"],
  "environment": {"PATH": "/usr/bin:/bin"},
  "inputs": ["hello.cc"],
  "outputs": ["hello"],
  "platform": {"toolchain": "my-gcc-image-v1", "OSFamily": "linux"},
  "timeout_seconds": 60,
  "no_cache": false
}
```

```sh
rush remote-execute action.json --execution-root . \
  --endpoint http://127.0.0.1:9000 --deadline-seconds 120 \
  --report outcome.json
./hello
```

Use `rexec` as the short command name. Declare every workspace dependency,
including scripts, response files and generated inputs. Inputs may be regular
files or directories; symlinks and paths containing dot or parent components
are rejected. The worker supplies absolute system tools. The manifest
inherits no client environment. Unknown fields fail parsing.

Rush snapshots input bytes to temporary disk storage and constructs a sorted
SHA256 Merkle tree. It uploads missing blobs using CAS batch updates or
ByteStream writes, looks up the action cache, and executes a miss. Command,
environment, input tree, output paths, timeout and platform participate in the
REAPI action digest. Set `no_cache` to true to bypass cache reads and request
uncached execution. Correct cache-write behavior also depends on the backend.

Rush requires every declared output, rejects extra or duplicate outputs and
reported directory/symlink outputs, and verifies sizes and SHA256 digests
before replacing local files. It preserves executable bits. Verification
failure preserves existing output files. Publication uses a rename per file;
a filesystem error during the rename phase can leave a partially published
multi-file result. A failed remote command returns its exit status and logs
without publishing outputs. Each captured stream is limited to 8 MiB.

The client deadline budgets connection, transfer, queueing and execution for
`remote-execute`; the action timeout bounds worker execution separately.
Deadline checks occur at asynchronous waits. Local snapshot copies, hashing
and output writes use blocking filesystem calls and can overrun that budget;
it is not a hard wall-time limit for stalled local storage.
Native build actions allow their execution timeout plus 120 seconds for
transfer and queueing, after connection. A disconnected client currently
cannot cancel a queued action: retain a finite worker timeout.

## Run a GPU action

Declare a worker platform that identifies the GPU runtime and device class.
Upload a script or program as an input and request its output files through
the same manifest interface. Provision GPU drivers, runtime libraries and
access control on the worker. Use the site's allocation mechanism before
starting workers with device access; Rush's platform properties select a
worker but do not reserve a physical GPU or measure free device memory.

Use [tests/reapi_gpu_probe.py](../tests/reapi_gpu_probe.py) for a bounded
1 MiB allocation, fill and readback check. Invoke it as `python3 probe.py --backend cuda --output result.json`
or `python3 probe.py --backend hip --device-name-contains DEVICE --output result.json`. Declare `probe.py`
as an input and `result.json` as an output. Set `no_cache: true` when checking
actual device execution. Read the [acceptance report](benchmarks/remote-execution.md)
before assuming that a particular runtime works.

## Define the trust and compatibility boundary

Isolate the entire worker service and its output collector, as well as the
command it launches. Keep SSH keys, model credentials, repository write
credentials and production mounts outside that boundary. A client cannot
detect a backend that follows an output symlink and reports the target as a
regular file. Qualification found this behavior in NativeLink 1.8.3; the
passing negative tests use isolation around the complete backend process.

Use dedicated scratch storage and bound worker/CAS capacity. Do not expose an
unauthenticated REAPI listener to an untrusted network. Trust worker images,
platform labels and action-cache writers: SHA256 verifies bytes against the
returned digest, not whether an authorized compiler produced them.

This initial interface accepts SHA256, uncompressed file blobs and regular
file outputs. Directory outputs, symlink transport, custom authentication
metadata, retries, `WaitExecution` recovery, cancellation, native remote
Kbuild replay and first-class depfile ingestion remain open. Complete declared
inputs are required; a remote compiler discovering a missing dependency does
not automatically repair the manifest or native analysis. TensorFlow,
Modular and Linux remote builds have not been qualified by the small C++
and GPU checks.
