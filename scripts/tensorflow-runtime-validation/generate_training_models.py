#!/usr/bin/env python3
"""Generate deterministic training GraphDefs, inputs, and CPU references."""

import argparse
import json
import os
from pathlib import Path

os.environ.setdefault("CUDA_VISIBLE_DEVICES", "-1")
os.environ.setdefault("TF_CPP_MIN_LOG_LEVEL", "1")
os.environ.setdefault("TF_ENABLE_ONEDNN_OPTS", "0")
os.environ.setdefault("TF_DETERMINISTIC_OPS", "1")

import numpy as np
import tensorflow as tf

tf.compat.v1.disable_eager_execution()


def relative(root: Path, path: Path) -> str:
    return str(path.relative_to(root))


def safe_name(name: str) -> str:
    return "".join(character if character.isalnum() else "_" for character in name)


def write_array(
    root: Path, path: Path, array: np.ndarray, dtype_name: str
) -> dict:
    path.parent.mkdir(parents=True, exist_ok=True)
    contiguous = np.ascontiguousarray(array)
    contiguous.tofile(path)
    return {
        "dtype": dtype_name,
        "shape": list(contiguous.shape),
        "path": relative(root, path),
    }


class Initializers:
    def __init__(self, seed: int) -> None:
        self.generator = np.random.default_rng(seed)

    def weight(self, shape: list[int], scale: float = 0.04) -> np.ndarray:
        return self.generator.normal(0.0, scale, size=shape).astype(np.float32)

    @staticmethod
    def bias(shape: list[int]) -> np.ndarray:
        return np.zeros(shape, dtype=np.float32)


def variable(name: str, value: np.ndarray) -> tf.Variable:
    return tf.compat.v1.get_variable(name, initializer=value, dtype=tf.float32)


def build_mlp(initializers: Initializers) -> tuple[tf.Tensor, tf.Tensor, tf.Tensor]:
    inputs = tf.compat.v1.placeholder(tf.float32, [None, 128], name="features")
    labels = tf.compat.v1.placeholder(tf.int32, [None], name="labels")
    w1 = variable("w1", initializers.weight([128, 256]))
    b1 = variable("b1", initializers.bias([256]))
    w2 = variable("w2", initializers.weight([256, 128]))
    b2 = variable("b2", initializers.bias([128]))
    wo = variable("wo", initializers.weight([128, 10]))
    bo = variable("bo", initializers.bias([10]))
    hidden1 = tf.nn.relu(tf.matmul(inputs, w1) + b1)
    hidden2 = tf.nn.relu(tf.matmul(hidden1, w2) + b2)
    logits = tf.matmul(hidden2, wo) + bo
    return inputs, labels, logits


def build_cnn(initializers: Initializers) -> tuple[tf.Tensor, tf.Tensor, tf.Tensor]:
    inputs = tf.compat.v1.placeholder(
        tf.float32, [None, 28, 28, 1], name="features"
    )
    labels = tf.compat.v1.placeholder(tf.int32, [None], name="labels")
    w1 = variable("conv1_w", initializers.weight([3, 3, 1, 8]))
    b1 = variable("conv1_b", initializers.bias([8]))
    w2 = variable("conv2_w", initializers.weight([3, 3, 8, 16]))
    b2 = variable("conv2_b", initializers.bias([16]))
    wo = variable("output_w", initializers.weight([7 * 7 * 16, 10]))
    bo = variable("output_b", initializers.bias([10]))
    hidden = tf.nn.relu(tf.nn.conv2d(inputs, w1, 1, "SAME") + b1)
    hidden = tf.nn.avg_pool2d(hidden, 2, 2, "SAME")
    hidden = tf.nn.relu(tf.nn.conv2d(hidden, w2, 1, "SAME") + b2)
    hidden = tf.nn.avg_pool2d(hidden, 2, 2, "SAME")
    hidden = tf.reshape(hidden, [-1, 7 * 7 * 16])
    logits = tf.matmul(hidden, wo) + bo
    return inputs, labels, logits


def build_attention(
    initializers: Initializers,
) -> tuple[tf.Tensor, tf.Tensor, tf.Tensor]:
    inputs = tf.compat.v1.placeholder(
        tf.float32, [None, 32, 64], name="features"
    )
    labels = tf.compat.v1.placeholder(tf.int32, [None], name="labels")
    wq = variable("query_w", initializers.weight([64, 64]))
    wk = variable("key_w", initializers.weight([64, 64]))
    wv = variable("value_w", initializers.weight([64, 64]))
    wo = variable("output_w", initializers.weight([64, 10]))
    bo = variable("output_b", initializers.bias([10]))
    query = tf.tensordot(inputs, wq, axes=1)
    key = tf.tensordot(inputs, wk, axes=1)
    value = tf.tensordot(inputs, wv, axes=1)
    scores = tf.matmul(query, key, transpose_b=True) * (64.0**-0.5)
    attention = tf.nn.softmax(scores, axis=-1)
    context = tf.matmul(attention, value)
    pooled = tf.reduce_mean(context + inputs, axis=1)
    logits = tf.matmul(pooled, wo) + bo
    return inputs, labels, logits


def build_training_graph(
    model_name: str, seed: int, learning_rate: float
) -> tuple[tf.Graph, dict]:
    graph = tf.Graph()
    with graph.as_default():
        tf.compat.v1.set_random_seed(seed)
        initializers = Initializers(seed)
        if model_name == "mlp":
            inputs, labels, logits = build_mlp(initializers)
            sample_shape = [128]
        elif model_name == "cnn":
            inputs, labels, logits = build_cnn(initializers)
            sample_shape = [28, 28, 1]
        elif model_name == "attention":
            inputs, labels, logits = build_attention(initializers)
            sample_shape = [32, 64]
        else:
            raise ValueError(f"unknown training model: {model_name}")
        per_example = tf.nn.sparse_softmax_cross_entropy_with_logits(
            labels=labels, logits=logits
        )
        loss = tf.reduce_mean(per_example, name="loss")
        optimizer = tf.compat.v1.train.GradientDescentOptimizer(learning_rate)
        train = optimizer.minimize(loss, name="train")
        variables = tf.compat.v1.trainable_variables()
        final_fetches = [
            tf.identity(item.read_value(), name=f"final/{safe_name(item.name)}")
            for item in variables
        ]
        init = tf.group(tf.compat.v1.global_variables_initializer(), name="init")
    return graph, {
        "input": inputs,
        "labels": labels,
        "loss": loss,
        "train": train,
        "init": init,
        "final_fetches": final_fetches,
        "sample_shape": sample_shape,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--steps", type=int, default=40)
    args = parser.parse_args()
    if args.steps <= 0:
        raise SystemExit("--steps must be positive")

    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    manifest = {
        "schema_version": 1,
        "generator": "generate_training_models.py",
        "tensorflow_version": tf.__version__,
        "controls": {
            "reference_device": "CPU",
            "reference_threads": 1,
            "onednn": False,
            "optimizer": "SGD",
            "steps": args.steps,
            "batches": [1, 8, 32],
            "repeated_fixed_batch": True,
        },
        "cases": [],
    }

    for model_index, (model_name, learning_rate) in enumerate(
        [("mlp", 0.05), ("cnn", 0.03), ("attention", 0.03)]
    ):
        seed = 2000 + model_index
        graph, nodes = build_training_graph(model_name, seed, learning_rate)
        graph_path = root / "models" / f"training_{model_name}.pb"
        graph_path.parent.mkdir(parents=True, exist_ok=True)
        graph_path.write_bytes(graph.as_graph_def(add_shapes=True).SerializeToString())

        generator = np.random.default_rng(3000 + model_index)
        all_inputs = generator.normal(
            0.0, 0.35, size=[32, *nodes["sample_shape"]]
        ).astype(np.float32)
        all_labels = (np.arange(32, dtype=np.int32) * 7 + model_index) % 10
        config = tf.compat.v1.ConfigProto(
            allow_soft_placement=True,
            device_count={"GPU": 0},
            inter_op_parallelism_threads=1,
            intra_op_parallelism_threads=1,
        )

        for batch in [1, 8, 32]:
            case_id = f"training_{model_name}-b{batch}"
            batch_inputs = all_inputs[:batch]
            batch_labels = all_labels[:batch]
            input_path = root / "inputs" / f"{case_id}.features.float32.bin"
            label_path = root / "inputs" / f"{case_id}.labels.int32.bin"
            input_entry = write_array(root, input_path, batch_inputs, "float")
            input_entry["name"] = nodes["input"].name
            label_entry = write_array(root, label_path, batch_labels, "int32")
            label_entry["name"] = nodes["labels"].name

            losses = []
            with tf.compat.v1.Session(graph=graph, config=config) as session:
                session.run(nodes["init"])
                feed = {
                    nodes["input"]: batch_inputs,
                    nodes["labels"]: batch_labels,
                }
                for _ in range(args.steps):
                    loss_value, _ = session.run(
                        [nodes["loss"], nodes["train"]], feed_dict=feed
                    )
                    losses.append(loss_value)
                final_values = session.run(nodes["final_fetches"], feed_dict=feed)

            loss_path = (
                root / "references" / case_id / "loss_series.float32.bin"
            )
            loss_entry = write_array(
                root, loss_path, np.asarray(losses, dtype=np.float32), "float"
            )
            loss_entry["name"] = nodes["loss"].name
            final_entries = []
            for tensor, value in zip(nodes["final_fetches"], final_values):
                value_path = (
                    root
                    / "references"
                    / case_id
                    / f"{safe_name(tensor.name)}.float32.bin"
                )
                entry = write_array(
                    root, value_path, np.asarray(value, dtype=np.float32), "float"
                )
                entry["name"] = tensor.name
                final_entries.append(entry)

            manifest["cases"].append(
                {
                    "id": case_id,
                    "kind": "training",
                    "model": model_name,
                    "batch": batch,
                    "graph": relative(root, graph_path),
                    "feeds": [input_entry, label_entry],
                    "init_targets": [nodes["init"].name],
                    "targets": [nodes["train"].name],
                    "series_fetches": [nodes["loss"].name],
                    "final_fetches": [
                        tensor.name for tensor in nodes["final_fetches"]
                    ],
                    "steps": args.steps,
                    "references": {
                        "series": [loss_entry],
                        "final": final_entries,
                    },
                }
            )

    (root / "training_manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
