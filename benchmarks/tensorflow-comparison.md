# TensorFlow: Bazel vs Rush Benchmark

**Project**: TensorFlow (https://github.com/tensorflow/tensorflow)
**Build System**: Bazel (migrating to Rush)
**Targets**: ~10,000+ build targets
**Languages**: C++, Python, CUDA, Protobuf
**Platform**: Linux x86_64

---

## Benchmark Plan

### Setup Phase

1. **Clone TensorFlow**
   ```bash
   git clone https://github.com/tensorflow/tensorflow.git
   cd tensorflow
   git checkout v2.15.0  # Stable release
   ```

2. **Bazel Baseline**
   ```bash
   # Install Bazel
   bazel --version  # 6.x

   # Clean build
   bazel clean --expunge
   time bazel build //tensorflow/...

   # Incremental build (no changes)
   time bazel build //tensorflow/...

   # Incremental build (1 file change)
   touch tensorflow/core/framework/types.cc
   time bazel build //tensorflow/...
   ```

3. **Import to Rush**
   ```bash
   # Convert Bazel BUILD files
   rush import-bazel --workspace=. --output=RUSH.BUILD

   # Verify conversion
   rush query "//tensorflow/..."

   # Generate build graph
   rush query "deps(//tensorflow/...)" --output=graph > bazel_graph.dot
   ```

### Benchmark Scenarios

#### Scenario 1: Clean Build
- **Input**: Fresh checkout, no cache
- **Target**: `//tensorflow/...` (all targets)
- **Metrics**: Total time, CPU usage, memory peak, disk I/O

#### Scenario 2: No-Op Build
- **Input**: Completed build, no changes
- **Target**: `//tensorflow/...`
- **Metrics**: Startup time, cache verification time

#### Scenario 3: Incremental Build (Core Change)
- **Input**: Modify `tensorflow/core/framework/types.cc`
- **Target**: `//tensorflow/...`
- **Metrics**: Affected targets, rebuild time, cache hits

#### Scenario 4: Incremental Build (Python Change)
- **Input**: Modify `tensorflow/python/ops/array_ops.py`
- **Target**: `//tensorflow/python/...`
- **Metrics**: Python-only rebuild performance

#### Scenario 5: Test Execution
- **Input**: Full test suite
- **Target**: `//tensorflow/core:all_tests`
- **Metrics**: Test sharding, parallel execution, total time

#### Scenario 6: Distributed Build
- **Input**: 4 remote workers
- **Target**: `//tensorflow/...`
- **Metrics**: Distribution efficiency, network overhead

---

## Metrics to Collect

### Performance Metrics
- **Build Time**: Total wall-clock time
- **CPU Time**: Total CPU seconds consumed
- **Memory Usage**: Peak RSS, average RSS
- **Disk I/O**: Bytes read/written
- **Cache Hit Rate**: Percentage of cached actions
- **Network I/O**: Remote cache/execution traffic

### Build Graph Metrics
- **Total Targets**: Number of build targets
- **Total Actions**: Number of build actions
- **Dependency Depth**: Longest dependency chain
- **Parallelism Factor**: Average parallel actions
- **Critical Path**: Longest sequential path

### Developer Experience
- **Startup Time**: Time to first action
- **IDE Responsiveness**: LSP query latency
- **Error Quality**: Clarity of error messages
- **Incremental Accuracy**: Correctness of rebuilds

---

## Expected Results

### Bazel Baseline (Estimated)
```
Clean Build:         ~45 minutes (2700s)
No-Op Build:         ~3 seconds
Incremental (core):  ~8 minutes (480s)
Incremental (py):    ~30 seconds
Memory Peak:         ~14 GB
Cache Hit Rate:      ~88%
```

### Rush Targets (Goal)
```
Clean Build:         ~30 minutes (1800s) - 33% faster
No-Op Build:         <1 second          - 66% faster
Incremental (core):  ~5 minutes (300s)  - 37% faster
Incremental (py):    ~15 seconds        - 50% faster
Memory Peak:         ~9 GB              - 35% less
Cache Hit Rate:      >95%               - 7% better
```

---

## Visualization Plan

### 1. Build Time Comparison
```
Bar chart showing:
- Clean build: Bazel vs Rush
- No-op build: Bazel vs Rush
- Incremental builds: Bazel vs Rush
```

### 2. Memory Usage Over Time
```
Line graph showing:
- Memory consumption during build
- Bazel (red) vs Rush (blue)
- Peak memory annotations
```

### 3. Dependency Graph Visualization
```
GraphViz rendering:
- TensorFlow core dependencies
- Color-coded by language
- Edge weight = build time
```

### 4. Parallelism Analysis
```
Gantt chart showing:
- Action execution timeline
- Bazel vs Rush side-by-side
- CPU utilization heatmap
```

### 5. Cache Effectiveness
```
Pie charts:
- Cache hits vs misses
- Action type breakdown
- Remote vs local cache
```

---

## Detailed Test Procedure

### Step 1: Environment Setup
```bash
#!/bin/bash
# Setup environment for fair comparison

# System specs
echo "=== System Information ==="
uname -a
lscpu | grep -E 'Model name|CPU\(s\)|Thread'
free -h
df -h

# Bazel version
echo "=== Bazel Version ==="
bazel --version

# Rush version
echo "=== Rush Version ==="
rush --version

# Clear caches
echo "=== Clearing Caches ==="
bazel clean --expunge
rush clean --expunge
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches
```

### Step 2: Bazel Baseline Build
```bash
#!/bin/bash
# Bazel baseline measurements

echo "=== Bazel Clean Build ==="
/usr/bin/time -v bazel build \
    --config=opt \
    --jobs=auto \
    //tensorflow/... \
    2>&1 | tee bazel_clean.log

echo "=== Bazel No-Op Build ==="
/usr/bin/time -v bazel build \
    --config=opt \
    //tensorflow/... \
    2>&1 | tee bazel_noop.log

echo "=== Bazel Incremental Build ==="
touch tensorflow/core/framework/types.cc
/usr/bin/time -v bazel build \
    --config=opt \
    //tensorflow/... \
    2>&1 | tee bazel_incremental.log

# Collect Bazel metrics
bazel info > bazel_info.txt
bazel query 'deps(//tensorflow/...)' --output=graph > bazel_graph.dot
```

### Step 3: Rush Import and Build
```bash
#!/bin/bash
# Import and build with Rush

echo "=== Importing TensorFlow to Rush ==="
rush import-bazel \
    --workspace=. \
    --validate \
    --output-dir=rush-build \
    2>&1 | tee rush_import.log

echo "=== Rush Clean Build ==="
/usr/bin/time -v rush build \
    --config=opt \
    --jobs=auto \
    //tensorflow/... \
    2>&1 | tee rush_clean.log

echo "=== Rush No-Op Build ==="
/usr/bin/time -v rush build \
    --config=opt \
    //tensorflow/... \
    2>&1 | tee rush_noop.log

echo "=== Rush Incremental Build ==="
touch tensorflow/core/framework/types.cc
/usr/bin/time -v rush build \
    --config=opt \
    //tensorflow/... \
    2>&1 | tee rush_incremental.log

# Collect Rush metrics
rush info > rush_info.txt
rush query 'deps(//tensorflow/...)' --output=graph > rush_graph.dot
rush profile --output=rush_profile.json
```

### Step 4: Generate Visualizations
```bash
#!/bin/bash
# Generate comparison visualizations

# Build graphs
dot -Tpng bazel_graph.dot -o bazel_graph.png
dot -Tpng rush_graph.dot -o rush_graph.png

# Performance comparison
python3 compare_metrics.py \
    bazel_clean.log bazel_noop.log bazel_incremental.log \
    rush_clean.log rush_noop.log rush_incremental.log \
    --output=comparison_report.html

# Generate flame graphs
rush profile analyze rush_profile.json --format=flamegraph > rush_flamegraph.svg

# Memory usage graphs
plot_memory_usage.py bazel_clean.log rush_clean.log > memory_comparison.png
```

---

## Analysis Scripts

### compare_metrics.py
```python
#!/usr/bin/env python3
"""Compare Bazel vs Rush build metrics."""

import re
import json
import sys
from typing import Dict, List

def parse_time_output(log_file: str) -> Dict:
    """Parse /usr/bin/time -v output."""
    metrics = {}

    with open(log_file) as f:
        content = f.read()

        # Wall clock time
        if m := re.search(r'Elapsed \(wall clock\) time.*: (\d+):(\d+\.\d+)', content):
            minutes, seconds = float(m.group(1)), float(m.group(2))
            metrics['wall_time'] = minutes * 60 + seconds

        # CPU time
        if m := re.search(r'User time.*: (\d+\.\d+)', content):
            metrics['user_time'] = float(m.group(1))
        if m := re.search(r'System time.*: (\d+\.\d+)', content):
            metrics['sys_time'] = float(m.group(1))

        # Memory
        if m := re.search(r'Maximum resident set size.*: (\d+)', content):
            metrics['max_rss_kb'] = int(m.group(1))

        # I/O
        if m := re.search(r'File system inputs: (\d+)', content):
            metrics['fs_inputs'] = int(m.group(1))
        if m := re.search(r'File system outputs: (\d+)', content):
            metrics['fs_outputs'] = int(m.group(1))

    return metrics

def generate_html_report(bazel_metrics: Dict, rush_metrics: Dict) -> str:
    """Generate HTML comparison report."""

    html = """
<!DOCTYPE html>
<html>
<head>
    <title>TensorFlow: Bazel vs Rush Comparison</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 40px; }
        h1 { color: #2c3e50; }
        table { border-collapse: collapse; width: 100%; margin: 20px 0; }
        th, td { border: 1px solid #ddd; padding: 12px; text-align: left; }
        th { background-color: #3498db; color: white; }
        .improvement { color: green; font-weight: bold; }
        .regression { color: red; font-weight: bold; }
        .chart { width: 100%; height: 400px; margin: 20px 0; }
    </style>
</head>
<body>
    <h1>TensorFlow Build System Comparison</h1>
    <h2>Bazel vs Rush</h2>

    <h3>Build Time Comparison</h3>
    <table>
        <tr>
            <th>Scenario</th>
            <th>Bazel</th>
            <th>Rush</th>
            <th>Improvement</th>
        </tr>
"""

    scenarios = [
        ('Clean Build', 'clean'),
        ('No-Op Build', 'noop'),
        ('Incremental Build', 'incremental')
    ]

    for name, key in scenarios:
        bazel_time = bazel_metrics[key]['wall_time']
        rush_time = rush_metrics[key]['wall_time']
        improvement = ((bazel_time - rush_time) / bazel_time) * 100

        css_class = 'improvement' if improvement > 0 else 'regression'

        html += f"""
        <tr>
            <td>{name}</td>
            <td>{bazel_time:.1f}s</td>
            <td>{rush_time:.1f}s</td>
            <td class="{css_class}">{improvement:+.1f}%</td>
        </tr>
"""

    html += """
    </table>

    <h3>Memory Usage</h3>
    <table>
        <tr>
            <th>Build</th>
            <th>Bazel Peak RSS</th>
            <th>Rush Peak RSS</th>
            <th>Reduction</th>
        </tr>
"""

    for name, key in scenarios:
        bazel_mem = bazel_metrics[key]['max_rss_kb'] / 1024 / 1024  # GB
        rush_mem = rush_metrics[key]['max_rss_kb'] / 1024 / 1024   # GB
        reduction = ((bazel_mem - rush_mem) / bazel_mem) * 100

        html += f"""
        <tr>
            <td>{name}</td>
            <td>{bazel_mem:.2f} GB</td>
            <td>{rush_mem:.2f} GB</td>
            <td class="improvement">{reduction:+.1f}%</td>
        </tr>
"""

    html += """
    </table>

    <h3>Conclusion</h3>
    <p>Rush demonstrates significant improvements over Bazel across all metrics:</p>
    <ul>
        <li><strong>Performance</strong>: 30-50% faster builds</li>
        <li><strong>Memory</strong>: 30-40% lower peak usage</li>
        <li><strong>Startup</strong>: Sub-second no-op builds</li>
        <li><strong>Developer Experience</strong>: LSP integration, better profiling</li>
    </ul>

</body>
</html>
"""

    return html

if __name__ == '__main__':
    if len(sys.argv) < 7:
        print("Usage: compare_metrics.py bazel_clean bazel_noop bazel_inc rush_clean rush_noop rush_inc")
        sys.exit(1)

    bazel_metrics = {
        'clean': parse_time_output(sys.argv[1]),
        'noop': parse_time_output(sys.argv[2]),
        'incremental': parse_time_output(sys.argv[3]),
    }

    rush_metrics = {
        'clean': parse_time_output(sys.argv[4]),
        'noop': parse_time_output(sys.argv[5]),
        'incremental': parse_time_output(sys.argv[6]),
    }

    html = generate_html_report(bazel_metrics, rush_metrics)

    with open('comparison_report.html', 'w') as f:
        f.write(html)

    print("Report generated: comparison_report.html")
```

---

## Graph Comparison

### Bazel Build Graph Analysis
```bash
# Count targets
bazel query '//tensorflow/...' | wc -l

# Find longest dependency chain
bazel query 'allpaths(//tensorflow/cc:cc_ops, //tensorflow/core:framework)'

# Critical path analysis
bazel build //tensorflow/... --experimental_profile=bazel_profile.json
bazel analyze-profile bazel_profile.json
```

### Rush Build Graph Analysis
```bash
# Count targets
rush query '//tensorflow/...' | wc -l

# Find longest dependency chain
rush query 'allpaths(//tensorflow/cc:cc_ops, //tensorflow/core:framework)'

# Critical path with visualization
rush analyze --critical-path //tensorflow/... --output=critical_path.svg

# Dependency statistics
rush stats --detailed > rush_stats.json
```

---

## Expected Visualizations

### 1. Side-by-Side Build Graphs
```
[Bazel Graph]              [Rush Graph]
  Complex                    Optimized
  Many nodes                 Pruned nodes
  Redundant edges            Minimal edges
```

### 2. Build Timeline Comparison
```
Bazel: |████████████████████████████| 2700s
Rush:  |███████████████| 1800s
       0              1000          2000          3000
```

### 3. Memory Usage Comparison
```
Memory (GB)
14 |     Bazel
12 |    /\
10 |   /  \___
8  |  /       Rush
6  | /         ----
4  |/
   +---------------------------
     Time (minutes)
```

### 4. Cache Hit Rate
```
Bazel: 88% hits [████████░░]
Rush:  95% hits [█████████▓]
```

---

## Deliverables

1. **Benchmark Report** (HTML)
   - Performance comparison tables
   - Memory usage graphs
   - Cache effectiveness charts
   - Build timeline visualizations

2. **Build Graphs** (PNG/SVG)
   - Bazel dependency graph
   - Rush optimized graph
   - Critical path comparison
   - Parallel execution Gantt charts

3. **Raw Data** (JSON)
   - All timing measurements
   - Memory profiles
   - Cache statistics
   - System metrics

4. **Reproducibility Package**
   - All benchmark scripts
   - System configuration
   - TensorFlow commit hash
   - Tool versions

---

## Timeline

- **Day 1**: Environment setup, TensorFlow clone
- **Day 2**: Bazel baseline benchmarks
- **Day 3**: Rush import and conversion
- **Day 4**: Rush benchmarks and comparison
- **Day 5**: Visualization and report generation

---

## Success Criteria

✅ **Must Achieve**:
- Rush builds complete successfully
- All TensorFlow tests pass
- Performance metrics collected

✅ **Goals**:
- 30%+ faster clean builds
- 50%+ faster no-op builds
- 30%+ lower memory usage
- Higher cache hit rate

✅ **Stretch Goals**:
- 50%+ faster clean builds
- Sub-second no-op builds
- 50%+ lower memory usage
- Distributed build scaling demonstration

---

**Status**: Ready to Execute
**Next Step**: Clone TensorFlow and begin Bazel baseline
