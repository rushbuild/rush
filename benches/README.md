# Rush Performance Benchmarks

This directory contains performance benchmarks for Rush, comparing it against Bazel where applicable.

## Benchmarks

### 1. Parsing Benchmarks (`bench_parsing.rs`)

Criterion-based benchmarks for core parsing functionality:

- **BUILD file parsing**: Tests parsing speed with varying numbers of rules (10, 50, 100, 500)
- **.bazelrc parsing**: Tests configuration file parsing with varying config counts (5, 10, 25, 50)
- **Compiler flags extraction**: Benchmarks extracting compiler options from bazelrc flags
- **Envoy macro translation**: Tests translation speed for Envoy-specific macros
- **Individual macro types**: Benchmarks each Envoy macro type separately

### 2. Bazel Comparison (`bench_vs_bazel.sh`)

Shell script that compares Rush and Bazel performance on real-world tasks:

- .bazelrc file parsing and processing
- BUILD file query and analysis
- Large BUILD file handling (100+ targets)

## Running Benchmarks

### Run all parsing benchmarks:

```bash
cargo bench
```

### Run specific benchmark:

```bash
cargo bench build_file_parsing
cargo bench bazelrc_parsing
cargo bench envoy_translation
```

### View HTML reports:

After running benchmarks, open the detailed HTML report:

```bash
open target/criterion/report/index.html
# or on Linux:
xdg-open target/criterion/report/index.html
```

### Run Bazel comparison:

```bash
./benches/bench_vs_bazel.sh
```

This will:
1. Check if Bazel is installed
2. Build Rush in release mode if needed
3. Create test projects and configurations
4. Time both tools on comparable tasks
5. Generate a comparison report in `benches/benchmark_results.txt`

## Benchmark Results Location

- **Criterion results**: `target/criterion/`
- **HTML reports**: `target/criterion/report/index.html`
- **Bazel comparison**: `benches/benchmark_results.txt`

## Adding New Benchmarks

To add a new parsing benchmark:

1. Add your benchmark function to `bench_parsing.rs`
2. Follow the criterion pattern (see existing benchmarks)
3. Add it to the `criterion_group!` macro at the bottom

To add a new comparison test:

1. Edit `bench_vs_bazel.sh`
2. Add your test in the appropriate section
3. Use the `time_command` function for consistent timing

## Performance Goals

Rush aims to be:

- **2-5x faster** than Bazel for cold builds (no JVM startup overhead)
- **Similar or faster** for incremental builds (same caching strategy)
- **10x faster** for analysis/query operations (native code vs JVM)

Current focus areas:
- Parsing speed (BUILD files, .bazelrc)
- Macro translation (Envoy compatibility)
- Cache lookup and validation
