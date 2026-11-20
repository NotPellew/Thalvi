#!/usr/bin/env python3
"""Draw a review chart from TLC's `-dump dot,actionlabels` output, without Java.

  python3 chart.py --dot <file.dot> --view <view.py> --out <chart.mmd> [--budget 300]

The hand-written view.py defines state(variables: dict[str, str]) -> str and
ACTIONS: dict[str, str | None]. Variables contain raw TLC values after dot
unescaping; wrapped values keep their newlines. Several TLC states may share
a chart name. None hides an action; an arrow from a chart state to itself is
dropped and equal (source, label, target) arrows are merged. Initial arrows
come first, then arrows sorted by source name, label, target name. Names
needing quotes get sorted s1, s2, ... declarations. Output is UTF-8
stateDiagram-v2 with LF lines.

Say what the reader sees or cares about. Put conditions in brackets in the
reader's words. Aim for about 20 arrows or fewer; fold detail into the view
instead of raising the budget. Names and labels must not contain a model's
variable or action name as a case-sensitive whole word. Every chart state
must be reachable from an initial state through visible arrows.

This is a review aid, separate from the check's result. The view is checked
before the budget, so a view error exits 2 even when the chart would be
skipped. Errors exit 2 and over-budget charts exit 0 with CHART skipped;
neither writes nor changes the --out file. A written chart exits 0 with
CHART written.
"""
import argparse
import importlib.util
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

ID = r"[+-]?[0-9]+"
NAME = r"[A-Za-z_][A-Za-z0-9_]*"
STATE = re.compile(rf'({ID}) \[label="((?:[^"\\]|\\["\\n])*)"(,style = filled\]|\];)')
STEP = re.compile(rf'({ID}) -> ({ID}) \[label="({NAME})",color="black",fontcolor="black"\];')
RANK = re.compile(rf'\{{rank = same; (?:{ID};)+\}}')
NUMBER = r"[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?"
VARIABLE = re.compile(rf"/\\ ({NAME}) = (.+)")


@dataclass
class TLCState:
    variables: dict[str, str]
    initial: bool


@dataclass
class Step:
    source: int
    target: int
    action: str
    line: int


@dataclass
class View:
    state: Callable[[dict[str, str]], str]
    actions: dict[str, str | None]


def variables(label: str, line: int) -> dict[str, str]:
    """Decode dot escapes once, then join TLC's indented value continuations."""
    decoded = re.sub(r'\\(["\\n])', lambda m: "\n" if m[1] == "n" else m[1], label)
    result = {}
    previous = None
    for part in decoded.split("\n"):
        variable = VARIABLE.fullmatch(part)
        if variable:
            name, value = variable.groups()
            if name in result:
                raise ValueError(f"line {line}: duplicate variable {name}")
            result[name] = value
            previous = name
        elif part.startswith(" ") and previous is not None:
            result[previous] += "\n" + part
        else:
            raise ValueError(f"line {line}: malformed variable label")
    return result


def parse_dot(path: Path) -> tuple[dict[int, TLCState], list[Step]]:
    """Read the TLC graph subset, retaining line numbers for malformed steps."""
    states, steps = {}, []
    opened = []
    graph_seen = False
    cluster_seen = False
    lines = path.read_bytes().decode("utf-8").split("\n")
    for line_number, raw in enumerate(lines, 1):
        line = raw.removesuffix("\r")
        if not line:
            continue
        if line == "strict digraph DiskGraph {" and not graph_seen:
            graph_seen = True
            opened.append("graph")
        elif re.fullmatch(rf"nodesep={NUMBER};", line) and opened == ["graph"]:
            continue
        elif line == "subgraph cluster_graph {" and opened == ["graph"] and not cluster_seen:
            cluster_seen = True
            opened.append("cluster")
        elif line == "}" and opened:
            opened.pop()
        elif opened == ["graph", "cluster"]:
            state = STATE.fullmatch(line)
            step = STEP.fullmatch(line)
            if line == 'color="white";' or RANK.fullmatch(line):
                continue
            if state:
                identifier, label, suffix = state.groups()
                identifier = int(identifier)
                if identifier in states:
                    raise ValueError(f"line {line_number}: duplicate state {identifier}")
                states[identifier] = TLCState(variables(label, line_number), suffix == ",style = filled]")
            elif step:
                source, target, action = step.groups()
                steps.append(Step(int(source), int(target), action, line_number))
            else:
                raise ValueError(f"line {line_number}: unrecognized TLC dot line")
        else:
            raise ValueError(f"line {line_number}: unexpected TLC dot line")
    if not graph_seen or not cluster_seen or opened or not states:
        raise ValueError(f"line {len(lines)}: incomplete TLC graph")
    for step in steps:
        for endpoint in (step.source, step.target):
            if endpoint not in states:
                raise ValueError(f"line {step.line}: step endpoint {endpoint} has no state")
    return states, steps


def load_view(path: Path) -> View:
    """Load and validate the external view's callable and action mapping."""
    spec = importlib.util.spec_from_file_location("_litany_chart_view", path)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot load view {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    state = getattr(module, "state", None)
    actions = getattr(module, "ACTIONS", None)
    if not callable(state):
        raise ValueError("view must define state(variables)")
    if not isinstance(actions, dict) or any(
        not isinstance(key, str) or (value is not None and not isinstance(value, str))
        for key, value in actions.items()
    ):
        raise ValueError("view ACTIONS must be a dict[str, str | None]")
    return View(state, actions)


def chart(states: dict[int, TLCState], steps: list[Step], view: View) -> tuple[str, int, int]:
    """Validate the view against the full model, then render deterministic bytes."""
    words = {name for state in states.values() for name in state.variables}
    words.update(step.action for step in steps)
    patterns = [(word, re.compile(rf"(?<![A-Za-z0-9_]){re.escape(word)}(?![A-Za-z0-9_])"))
                for word in sorted(words)]

    def check_words(text: str):
        for word, pattern in patterns:
            if pattern.search(text):
                raise ValueError(f"chart text {text!r} contains model name {word}")

    for action in sorted({step.action for step in steps}):
        if action not in view.actions:
            raise ValueError(f"ACTIONS lacks action {action}")
    for label in view.actions.values():
        if label is not None:
            check_words(label)

    names = {}
    for identifier, state in states.items():
        name = view.state(state.variables)
        if not isinstance(name, str) or not name:
            raise ValueError(f"state() must return a nonempty name for TLC state {identifier}")
        if '"' in name:
            raise ValueError(f"chart state {name!r} contains a double quote")
        check_words(name)
        names[identifier] = name
    all_names = set(names.values())
    starts = {names[identifier] for identifier, state in states.items() if state.initial}
    arrows = set()
    for step in steps:
        label = view.actions[step.action]
        source, target = names[step.source], names[step.target]
        if label is None or source == target:
            continue
        arrows.add((source, label, target))

    outgoing = {name: set() for name in all_names}
    for source, label, target in arrows:
        outgoing[source].add(target)
    reachable = set(starts)
    pending = list(starts)
    while pending:
        for target in outgoing[pending.pop()] - reachable:
            reachable.add(target)
            pending.append(target)
    if all_names - reachable:
        raise ValueError("unreachable chart states: " + ", ".join(sorted(all_names - reachable)))

    quoted = sorted(name for name in all_names if not re.fullmatch(r"[A-Za-z0-9_]+", name))
    aliases = {name: f"s{index}" for index, name in enumerate(quoted, 1)}
    identifiers = {name: aliases.get(name, name) for name in all_names}
    lines = ["stateDiagram-v2"]
    lines.extend(f'  state "{name}" as {aliases[name]}' for name in quoted)
    lines.extend(f"  [*] --> {identifiers[name]}" for name in sorted(starts))
    lines.extend(f"  {identifiers[source]} --> {identifiers[target]}: {label}"
                 for source, label, target in sorted(arrows))
    return "\n".join(lines) + "\n", len(all_names), len(arrows)


class Arguments(argparse.ArgumentParser):
    def error(self, message):
        self.exit(2, f"chart: {message}\n")


def main(argv=None) -> int:
    parser = Arguments(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dot", type=Path, required=True)
    parser.add_argument("--view", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--budget", type=int, default=300)
    args = parser.parse_args(argv)
    try:
        states, steps = parse_dot(args.dot)
        text, state_count, arrow_count = chart(states, steps, load_view(args.view))
        if state_count > args.budget:
            print(f"CHART skipped states={state_count} budget={args.budget}")
            return 0
        args.out.write_bytes(text.encode("utf-8"))
        print(f"CHART written states={state_count} arrows={arrow_count} path={args.out}")
        return 0
    except (Exception, SystemExit) as error:
        print(f"chart: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    sys.exit(main())
