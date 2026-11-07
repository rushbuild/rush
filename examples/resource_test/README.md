# Resource Manager Test

Simple C++ hello world to test Rush's new dynamic resource manager.

## Build

```bash
cd examples/resource_test
../../target/release/rush build //:hello
```

## Run

```bash
./rush-out/fastbuild/bin/hello
```

## What This Tests

- Dynamic memory allocation tracking (6GB per C++ compile)
- Parallel compilation with resource constraints
- Resource statistics reporting
- Multi-library linking

The build should show peak memory usage at completion.
