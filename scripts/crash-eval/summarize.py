#!/usr/bin/env python3
"""Summarize a crash-eval run: pass rate and cost per arm, and each crash
arm's overhead against the baseline, paired by task.

    summarize.py OUT        (OUT as given to run-suite.sh)

Stdlib only (macOS ships Python 3.9).
"""

import glob
import json
import os
import statistics
import sys


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(value, digits=0):
    if value is None:
        return "—"
    if isinstance(value, float) and digits:
        return f"{value:.{digits}f}"
    return f"{round(value):,}"


def main(out):
    results = []
    for path in glob.glob(os.path.join(out, "*", "*", "*", "result.json")):
        try:
            results.append(json.load(open(path)))
        except (OSError, ValueError):
            print(f"skipping unreadable {path}", file=sys.stderr)
    if not results:
        sys.exit(f"no result.json under {out}")

    arms = [a for a in ("baseline", "resume", "cold") if any(r["arm"] == a for r in results)]
    by = {}
    for r in results:
        by.setdefault((r["task"], r["arm"]), []).append(r)
    tasks = sorted({r["task"] for r in results})
    model = sorted({r["model"] for r in results})

    print(f"# Crash-injection eval\n\nModel: {', '.join(model)} · {len(tasks)} tasks · {len(results)} runs\n")

    print("## Per arm\n")
    print("| Arm | Runs | Pass | Median wall (s) | Median tokens | Median model calls | Median tool calls | Repeated commands (mean) |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- |")
    for arm in arms:
        rs = [r for r in results if r["arm"] == arm]
        passed = sum(r["pass"] for r in rs)
        print(
            f"| {arm} | {len(rs)} | {passed}/{len(rs)} ({100 * passed / len(rs):.0f}%) "
            f"| {fmt(median([r['wallSecs'] for r in rs]))} "
            f"| {fmt(median([r['totalTokens'] for r in rs]))} "
            f"| {fmt(median([r['modelRequests'] for r in rs]))} "
            f"| {fmt(median([r['toolCalls'] for r in rs]))} "
            f"| {statistics.mean(r['repeatedCommands'] for r in rs):.1f} |"
        )

    if "baseline" in arms:
        print("\n## Overhead against baseline (median over tasks of the per-task median ratio)\n")
        print("| Arm | Tasks | Wall × | Tokens × | Model calls × |")
        print("| --- | --- | --- | --- | --- |")
        for arm in arms:
            if arm == "baseline":
                continue
            ratios = {"wallSecs": [], "totalTokens": [], "modelRequests": []}
            paired = 0
            for task in tasks:
                base, other = by.get((task, "baseline")), by.get((task, arm))
                if not base or not other:
                    continue
                paired += 1
                for key in ratios:
                    b, o = median([r[key] for r in base]), median([r[key] for r in other])
                    if b:
                        ratios[key].append(o / b)
            print(
                f"| {arm} | {paired} | {fmt(median(ratios['wallSecs']), 2)} "
                f"| {fmt(median(ratios['totalTokens']), 2)} | {fmt(median(ratios['modelRequests']), 2)} |"
            )

    print("\n## Per task\n")
    print("| Task | " + " | ".join(arms) + " |")
    print("| --- | " + " | ".join("---" for _ in arms) + " |")
    for task in tasks:
        cells = []
        for arm in arms:
            rs = by.get((task, arm), [])
            if not rs:
                cells.append("—")
                continue
            passed = sum(r["pass"] for r in rs)
            cells.append(
                f"{passed}/{len(rs)} · {fmt(median([r['wallSecs'] for r in rs]))}s "
                f"· {fmt(median([r['totalTokens'] for r in rs]))} tok"
            )
        print(f"| {task} | " + " | ".join(cells) + " |")

    declined = [r for r in results if r["arm"] == "resume" and False in (r.get("relaunched") or [])]
    if declined:
        print(f"\n{len(declined)} resume run(s) where the engine did not relaunch the Turn:")
        for r in declined:
            print(f"- {r['task']}: final status {r['finalStatus']}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
