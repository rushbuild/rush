#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Plot accepted package timings with their observed minimum/maximum ranges."""
import argparse
import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    data = json.loads(args.data.read_text())
    plt.rcParams.update({'svg.hashsalt': 'rush-modular-cpu-20261109',
                         'svg.fonttype': 'none', 'font.size': 10,
                         'text.color': '#f1f2e9', 'axes.labelcolor': '#f1f2e9',
                         'xtick.color': '#adc0bc', 'ytick.color': '#adc0bc',
                         'axes.edgecolor': '#344649'})
    fig, axes = plt.subplots(1, 3, figsize=(13, 4.6), facecolor='#101719')
    engines = [('rush-before', 'Rush before', '#879b99'),
               ('rush-after', 'Rush current', '#ffad5a'),
               ('bazel', 'Bazel', '#65c3b7')]
    hosts = data['hosts']
    for ax, (mode, title, scale, unit) in zip(axes, [
            ('clean', 'Clear engine outputs/cache · n=3', 1, 'seconds'),
            ('noop', 'Unchanged inputs · n=10', 1000, 'milliseconds · log scale'),
            ('novel-edit', 'Comment edit · n=3', 1, 'seconds')]):
        ax.set_facecolor('#182326')
        for index, (engine, label, color) in enumerate(engines):
            values = [h['summary'][mode][engine] for h in hosts]
            medians = np.array([v['median_seconds'] * scale for v in values])
            bounds = np.array([[v['min_seconds'] * scale for v in values],
                               [v['max_seconds'] * scale for v in values]])
            error = np.array([medians - bounds[0], bounds[1] - medians])
            positions = np.arange(len(hosts)) + (index - 1) * .24
            ax.bar(positions, medians, .22, color=color, label=label,
                   yerr=error, error_kw={'ecolor': '#f1f2e9', 'capsize': 3})
        ax.set_xticks(np.arange(len(hosts)), [h['host_label'] for h in hosts])
        ax.set_title(title, color='#f1f2e9', fontsize=10)
        ax.set_ylabel(unit)
        ax.grid(axis='y', alpha=.15)
        if mode == 'noop':
            ax.set_yscale('log')
        else:
            ax.set_ylim(bottom=0)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc='upper center', ncol=3,
               bbox_to_anchor=(.5, .94), facecolor='#182326', edgecolor='#344649')
    fig.suptitle('Modular stdlib package · CPU8 · median and observed range',
                 color='#f1f2e9', fontsize=14, y=.995)
    fig.text(.5, .015, 'One pinned package; compiler calls and output bytes checked. '
             'Ranges are minima/maxima, not confidence intervals.',
             ha='center', color='#adc0bc', fontsize=9)
    fig.tight_layout(rect=(0, .055, 1, .85))
    fig.savefig(args.output, facecolor=fig.get_facecolor(),
                metadata={'Date': None, 'Creator': 'Rush workload comparison'})
    plt.close(fig)
    args.output.write_text("\n".join(line.rstrip() for line in
                                     args.output.read_text().splitlines()) + "\n")


if __name__ == '__main__':
    main()
