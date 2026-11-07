#!/usr/bin/env python3
"""
Visualize Multi-AI Collaboration for Rush Build System
Reads .ai-traces/session-map.json and generates visual representations
"""

import json
import sys
from pathlib import Path
from datetime import datetime
from typing import Dict, List

def load_session_map(path: Path) -> Dict:
    """Load session map JSON"""
    with open(path, 'r') as f:
        return json.load(f)

def generate_ascii_timeline(sessions: List[Dict]) -> str:
    """Generate ASCII art timeline of AI collaboration"""

    output = []
    output.append("=" * 80)
    output.append("Rush AI Collaboration Timeline")
    output.append("=" * 80)
    output.append("")

    # Group by AI
    by_ai = {}
    for session in sessions:
        ai = session['ai']
        if ai not in by_ai:
            by_ai[ai] = []
        by_ai[ai].append(session)

    # Timeline
    for i, session in enumerate(sessions, 1):
        ai = session['ai']
        task = session['task_type']
        tokens = session.get('tokens', 0)

        # Color coding with ANSI
        ai_color = {
            'Claude-Code': '\033[94m',     # Blue
            'ChatGPT-Codex': '\033[92m',   # Green
            'Gemini-CLI': '\033[93m',      # Yellow
        }.get(ai, '\033[0m')
        reset = '\033[0m'

        output.append(f"[{i}] {ai_color}{ai:15}{reset} │ {task:10} │ {tokens:6,} tokens")

        # Show files modified
        if 'files_modified' in session:
            files = session['files_modified']
            if len(files) <= 3:
                for f in files:
                    output.append(f"    └─ {f}")
            else:
                output.append(f"    └─ {len(files)} files modified")

        # Show handoffs
        if session.get('handoff_to'):
            output.append(f"    ↓ handoff to → {session['handoff_to']}")

        output.append("")

    return "\n".join(output)

def generate_ascii_graph(sessions: List[Dict]) -> str:
    """Generate ASCII dependency graph"""

    output = []
    output.append("")
    output.append("AI Collaboration Graph")
    output.append("=" * 60)
    output.append("")

    # Build dependency graph
    graph = {}
    for session in sessions:
        sid = session['id']
        graph[sid] = {
            'ai': session['ai'],
            'task': session['task_type'],
            'handoff_from': session.get('handoff_from'),
            'handoff_to': session.get('handoff_to'),
            'review_by': session.get('review_by', [])
        }

    # Visualize
    for sid, info in graph.items():
        ai_symbol = {
            'Claude-Code': '🤖',
            'ChatGPT-Codex': '⚙️',
            'Gemini-CLI': '✨',
        }.get(info['ai'], '📦')

        output.append(f"{ai_symbol} {sid}")
        output.append(f"   Task: {info['task']}")

        if info['handoff_from']:
            output.append(f"   ↑ from: {info['handoff_from']}")

        if info['handoff_to']:
            output.append(f"   ↓ to: {info['handoff_to']}")

        if info['review_by']:
            output.append(f"   👁  reviewed by: {', '.join(info['review_by'])}")

        output.append("")

    return "\n".join(output)

def generate_stats(session_map: Dict) -> str:
    """Generate statistics summary"""

    sessions = session_map.get('sessions', [])

    output = []
    output.append("")
    output.append("=" * 80)
    output.append("Collaboration Statistics")
    output.append("=" * 80)
    output.append("")

    # Token usage by AI
    tokens_by_ai = {}
    for session in sessions:
        ai = session['ai']
        tokens = session.get('tokens', 0)
        tokens_by_ai[ai] = tokens_by_ai.get(ai, 0) + tokens

    total_tokens = sum(tokens_by_ai.values())

    output.append("Token Usage by AI:")
    output.append("-" * 40)
    for ai, tokens in sorted(tokens_by_ai.items(), key=lambda x: -x[1]):
        pct = (tokens / total_tokens * 100) if total_tokens > 0 else 0
        bar_len = int(pct / 2)
        bar = "█" * bar_len
        output.append(f"  {ai:15} {tokens:8,} tokens {bar} {pct:5.1f}%")
    output.append(f"  {'TOTAL':15} {total_tokens:8,} tokens")
    output.append("")

    # Task types
    task_counts = {}
    for session in sessions:
        task = session['task_type']
        task_counts[task] = task_counts.get(task, 0) + 1

    output.append("Tasks by Type:")
    output.append("-" * 40)
    for task, count in sorted(task_counts.items()):
        output.append(f"  {task:15} {count:3} tasks")
    output.append("")

    # Efficiency metrics
    if 'parallel_execution' in session_map:
        output.append("Execution:")
        output.append("-" * 40)
        output.append(f"  Parallel: {'Yes' if session_map['parallel_execution'] else 'No'}")
        if 'time_saved_minutes' in session_map:
            output.append(f"  Time saved: {session_map['time_saved_minutes']} minutes")
        output.append("")

    return "\n".join(output)

def generate_graphviz(sessions: List[Dict]) -> str:
    """Generate Graphviz DOT format for rendering to SVG"""

    output = []
    output.append("digraph AICollaboration {")
    output.append("  rankdir=TB;")
    output.append("  node [shape=box, style=rounded];")
    output.append("")

    # Define nodes
    for session in sessions:
        sid = session['id']
        ai = session['ai']
        task = session['task_type']
        tokens = session.get('tokens', 0)

        # Color by AI
        color = {
            'Claude-Code': 'lightblue',
            'ChatGPT-Codex': 'lightgreen',
            'Gemini-CLI': 'lightyellow',
        }.get(ai, 'white')

        label = f"{ai}\\n{task}\\n{tokens:,} tokens"
        output.append(f'  "{sid}" [label="{label}", fillcolor={color}, style=filled];')

    output.append("")

    # Define edges
    for session in sessions:
        sid = session['id']

        # Handoff edges
        if session.get('handoff_from'):
            output.append(f'  "{session["handoff_from"]}" -> "{sid}" [label="handoff"];')

        # Review edges
        for reviewer in session.get('review_by', []):
            output.append(f'  "{sid}" -> "{reviewer}-review" [label="review", style=dashed];')

    output.append("}")

    return "\n".join(output)

def main():
    """Main entry point"""

    # Find session map
    session_map_path = Path(__file__).parent.parent / ".ai-traces" / "session-map.json"

    if not session_map_path.exists():
        print(f"Error: {session_map_path} not found", file=sys.stderr)
        print("Run this script from the Rush project root directory.", file=sys.stderr)
        sys.exit(1)

    # Load data
    session_map = load_session_map(session_map_path)
    sessions = session_map.get('sessions', [])

    if not sessions:
        print("No sessions found in session-map.json", file=sys.stderr)
        sys.exit(1)

    # Generate visualizations
    print(generate_ascii_timeline(sessions))
    print(generate_ascii_graph(sessions))
    print(generate_stats(session_map))

    # Optionally generate Graphviz
    if '--graphviz' in sys.argv or '--dot' in sys.argv:
        dot_output = generate_graphviz(sessions)
        print("\n" + "=" * 80)
        print("Graphviz DOT format (save to file.dot and run: dot -Tsvg file.dot -o output.svg)")
        print("=" * 80)
        print(dot_output)

    if '--svg' in sys.argv:
        try:
            import subprocess
            dot_output = generate_graphviz(sessions)
            output_file = "ai-collaboration.svg"

            proc = subprocess.run(
                ['dot', '-Tsvg', '-o', output_file],
                input=dot_output.encode(),
                capture_output=True
            )

            if proc.returncode == 0:
                print(f"\n✅ SVG visualization saved to: {output_file}")
            else:
                print(f"\n❌ Error generating SVG: {proc.stderr.decode()}", file=sys.stderr)
                print("Make sure graphviz is installed: sudo apt install graphviz", file=sys.stderr)
        except FileNotFoundError:
            print("\n❌ Graphviz not found. Install with: sudo apt install graphviz", file=sys.stderr)

if __name__ == '__main__':
    main()
