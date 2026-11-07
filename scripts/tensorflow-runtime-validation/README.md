# TensorFlow runtime validation

This package tests artifacts built by Rush and Bazel, rather than treating a
successful link as proof of runtime correctness.

The same `tf_parity_runner.cc` is copied into a TensorFlow checkout and compiled
once by each build system. Both binaries load byte-identical GraphDefs and raw
input tensors. The comparator checks:

- Rush output against Bazel output with strict tolerances.
- Both outputs against a canonical TensorFlow CPU reference.
- Classification top-1 agreement where the output is a logits matrix.
- Fixed-seed training loss trajectories and final trainable tensors.
- Training convergence and finite values.
- Median and median absolute deviation for each inference/training case.

The inference matrix is Inception-v1 at batch 1 plus ResNet50, MobileNetV2, and
a transformer-style encoder at batches 1, 8, and 32. The training matrix is an
MLP, CNN, and attention model at batches 1, 8, and 32 for 40 fixed-data SGD
steps.

## Prepare artifacts

Use TensorFlow 2.15 in an isolated Python environment:

```bash
python generate_inference_models.py \
  --out /data/rush-runtime-validation/artifacts \
  --inception-graph /path/to/tensorflow_inception_graph.pb
python generate_training_models.py \
  --out /data/rush-runtime-validation/artifacts
```

## Install and build the common runner

```bash
mkdir -p /path/to/tensorflow/tensorflow/rush_validation
cp BUILD tf_parity_runner.cc \
  /path/to/tensorflow/tensorflow/rush_validation/

# Build this target independently with each build system and identical CUDA,
# compiler, optimization, and TensorFlow configuration.
//tensorflow/rush_validation:tf_parity_runner
```

## Run and compare

```bash
python run_matrix.py \
  --runner /path/to/rush-built/tf_parity_runner \
  --artifacts /data/rush-runtime-validation/artifacts \
  --out /data/rush-runtime-validation/rush \
  --require-gpu

python run_matrix.py \
  --runner /path/to/bazel-built/tf_parity_runner \
  --artifacts /data/rush-runtime-validation/artifacts \
  --out /data/rush-runtime-validation/bazel \
  --require-gpu

python compare_results.py \
  --rush /data/rush-runtime-validation/rush \
  --bazel /data/rush-runtime-validation/bazel \
  --artifacts /data/rush-runtime-validation/artifacts \
  --out /data/rush-runtime-validation/comparison
```
