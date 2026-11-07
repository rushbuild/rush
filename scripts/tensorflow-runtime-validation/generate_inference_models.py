#!/usr/bin/env python3
"""Generate deterministic inference GraphDefs, inputs, and CPU references."""

import argparse
import json
import os
import shutil
from pathlib import Path

os.environ.setdefault("CUDA_VISIBLE_DEVICES", "-1")
os.environ.setdefault("TF_CPP_MIN_LOG_LEVEL", "1")
os.environ.setdefault("TF_ENABLE_ONEDNN_OPTS", "0")

import numpy as np
import tensorflow as tf
from tensorflow.python.framework.convert_to_constants import (
    convert_variables_to_constants_v2,
)


def relative(root: Path, path: Path) -> str:
    return str(path.relative_to(root))


def safe_name(name: str) -> str:
    return "".join(character if character.isalnum() else "_" for character in name)


def write_array(root: Path, path: Path, array: np.ndarray) -> dict:
    path.parent.mkdir(parents=True, exist_ok=True)
    contiguous = np.ascontiguousarray(array)
    contiguous.tofile(path)
    return {
        "dtype": "float",
        "shape": list(contiguous.shape),
        "path": relative(root, path),
    }


def deterministic_input(shape: list[int], seed: int) -> np.ndarray:
    generator = np.random.default_rng(seed)
    return generator.uniform(-1.0, 1.0, size=shape).astype(np.float32)


def assign_deterministic_weights(model: tf.keras.Model, seed: int) -> None:
    generator = np.random.default_rng(seed)
    for variable in model.variables:
        lowered = variable.name.lower()
        shape = tuple(variable.shape)
        if "moving_variance" in lowered or lowered.endswith("gamma:0"):
            value = np.ones(shape, dtype=np.float32)
        elif "moving_mean" in lowered or lowered.endswith("beta:0"):
            value = np.zeros(shape, dtype=np.float32)
        else:
            value = generator.normal(0.0, 0.025, size=shape).astype(np.float32)
        variable.assign(value)


class TinyTransformer(tf.keras.Model):
    def __init__(self) -> None:
        super().__init__()
        self.query = tf.keras.layers.Dense(128, use_bias=False)
        self.key = tf.keras.layers.Dense(128, use_bias=False)
        self.value = tf.keras.layers.Dense(128, use_bias=False)
        self.projection = tf.keras.layers.Dense(128)
        self.normalization = tf.keras.layers.LayerNormalization(epsilon=1e-5)
        self.classifier = tf.keras.layers.Dense(64)

    def call(self, inputs: tf.Tensor) -> tf.Tensor:
        batch = tf.shape(inputs)[0]

        def heads(layer: tf.keras.layers.Layer) -> tf.Tensor:
            value = layer(inputs)
            value = tf.reshape(value, [batch, 64, 4, 32])
            return tf.transpose(value, [0, 2, 1, 3])

        query = heads(self.query)
        key = heads(self.key)
        value = heads(self.value)
        scores = tf.matmul(query, key, transpose_b=True) * (32.0**-0.5)
        attention = tf.nn.softmax(scores, axis=-1)
        context = tf.matmul(attention, value)
        context = tf.transpose(context, [0, 2, 1, 3])
        context = tf.reshape(context, [batch, 64, 128])
        encoded = self.normalization(inputs + self.projection(context))
        return self.classifier(tf.reduce_mean(encoded, axis=1))


def freeze_model(name: str) -> tuple[tf.compat.v1.GraphDef, str, list[str]]:
    if name == "resnet50":
        model = tf.keras.applications.ResNet50(
            include_top=True, weights=None, classes=1000
        )
        input_shape = [None, 224, 224, 3]
        seed = 501
    elif name == "mobilenet_v2":
        model = tf.keras.applications.MobileNetV2(
            include_top=True, weights=None, classes=1000
        )
        input_shape = [None, 224, 224, 3]
        seed = 502
    elif name == "tiny_transformer":
        model = TinyTransformer()
        input_shape = [None, 64, 128]
        seed = 503
    else:
        raise ValueError(f"unknown model: {name}")

    model(tf.zeros([1, *input_shape[1:]], dtype=tf.float32), training=False)
    assign_deterministic_weights(model, seed)

    @tf.function(
        input_signature=[
            tf.TensorSpec(input_shape, tf.float32, name="validation_input")
        ]
    )
    def serving(inputs: tf.Tensor) -> tf.Tensor:
        return tf.identity(model(inputs, training=False), name="validation_output")

    frozen = convert_variables_to_constants_v2(serving.get_concrete_function())
    graph = frozen.graph.as_graph_def(add_shapes=True)
    return graph, frozen.inputs[0].name, [frozen.outputs[0].name]


def graph_reference(
    graph_def: tf.compat.v1.GraphDef,
    input_name: str,
    output_names: list[str],
    input_array: np.ndarray,
) -> list[np.ndarray]:
    graph = tf.Graph()
    with graph.as_default():
        tf.import_graph_def(graph_def, name="")
    config = tf.compat.v1.ConfigProto(
        allow_soft_placement=True,
        device_count={"GPU": 0},
        inter_op_parallelism_threads=1,
        intra_op_parallelism_threads=1,
    )
    with tf.compat.v1.Session(graph=graph, config=config) as session:
        return session.run(output_names, feed_dict={input_name: input_array})


def add_cases(
    root: Path,
    manifest: dict,
    model_name: str,
    graph_def: tf.compat.v1.GraphDef,
    graph_path: Path,
    input_name: str,
    output_names: list[str],
    sample_shape: list[int],
    batches: list[int],
    seed: int,
) -> None:
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_bytes(graph_def.SerializeToString())
    all_inputs = deterministic_input([max(batches), *sample_shape], seed)
    for batch in batches:
        case_id = f"{model_name}-b{batch}"
        case_input = all_inputs[:batch]
        input_path = root / "inputs" / f"{case_id}.float32.bin"
        input_entry = write_array(root, input_path, case_input)
        input_entry["name"] = input_name
        references = graph_reference(
            graph_def, input_name, output_names, case_input
        )
        reference_entries = []
        for output_name, output in zip(output_names, references):
            output_path = (
                root
                / "references"
                / case_id
                / f"{safe_name(output_name)}.float32.bin"
            )
            entry = write_array(root, output_path, np.asarray(output, np.float32))
            entry["name"] = output_name
            reference_entries.append(entry)
        manifest["cases"].append(
            {
                "id": case_id,
                "kind": "inference",
                "model": model_name,
                "batch": batch,
                "graph": relative(root, graph_path),
                "feeds": [input_entry],
                "series_fetches": [],
                "final_fetches": output_names,
                "references": {"final": reference_entries},
            }
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--inception-graph", required=True, type=Path)
    parser.add_argument("--inception-input", default="input:0")
    parser.add_argument("--inception-output", default="output:0")
    args = parser.parse_args()

    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    tf.keras.utils.set_random_seed(20260727)
    manifest = {
        "schema_version": 1,
        "generator": "generate_inference_models.py",
        "tensorflow_version": tf.__version__,
        "controls": {
            "reference_device": "CPU",
            "reference_threads": 1,
            "onednn": False,
            "input_distribution": "PCG64 uniform[-1,1]",
        },
        "cases": [],
    }

    inception_graph = tf.compat.v1.GraphDef()
    inception_graph.ParseFromString(args.inception_graph.read_bytes())
    inception_path = root / "models" / "inception_v1.pb"
    if args.inception_graph.resolve() != inception_path.resolve():
        inception_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(args.inception_graph, inception_path)
    add_cases(
        root,
        manifest,
        "inception_v1",
        inception_graph,
        inception_path,
        args.inception_input,
        [args.inception_output],
        [224, 224, 3],
        [1],
        1001,
    )

    for name, shape, batches, seed in [
        ("resnet50", [224, 224, 3], [1, 8, 32], 1002),
        ("mobilenet_v2", [224, 224, 3], [1, 8, 32], 1003),
        ("tiny_transformer", [64, 128], [1, 8, 32], 1004),
    ]:
        graph_def, input_name, output_names = freeze_model(name)
        add_cases(
            root,
            manifest,
            name,
            graph_def,
            root / "models" / f"{name}.pb",
            input_name,
            output_names,
            shape,
            batches,
            seed,
        )
        tf.keras.backend.clear_session()

    (root / "inference_manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
