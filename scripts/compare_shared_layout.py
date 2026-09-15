#!/usr/bin/env python3
"""Alternate immutable Rust acceptance binaries without scanning the filesystem.

Both binaries must contain the identical compact_acceptance_tests harness.
Output contains aggregate counters and fingerprints, never indexed file paths.
The output directory must be new; immutable inputs are never overwritten.
"""
import argparse
import json
import math
import os
from pathlib import Path
import statistics
import subprocess


def percentile(samples, fraction=0.95):
    return sorted(samples)[math.ceil(len(samples) * fraction) - 1]


def summarize_comparison(records, rounds, repetitions):
    fingerprints = {r["metadata_digest"] for runs in records.values() for r in runs}
    if len(fingerprints) != 1:
        raise AssertionError("Corpora do not match")
    cases = []
    for index in range(len(records["baseline"][0]["queries"])):
        rows = {side: [run["queries"][index] for run in runs] for side, runs in records.items()}
        if len({row["digest"] for runs in rows.values() for row in runs}) != 1:
            raise AssertionError(f"Case {index} differs in result or order")
        limit = 15 if index in (14, 19, 20) else 5
        modes = {}
        for mode in ("uncached", "warm"):
            values = {side: percentile([v for row in runs for v in row[mode]["samples_ms"]]) for side, runs in rows.items()}
            delta = (values["candidate"] / values["baseline"] - 1) * 100
            modes[mode] = values | {"regression_percent": delta, "passes": delta <= limit}
        cases.append({"case": index, "allowed_regression_percent": limit, **modes})
    summary = {
        "rounds": rounds, "repetitions_per_mode_per_round": repetitions,
        "scope": "Core restore and query; excludes XPC, UI and event reconciliation",
        "same_corpus_and_pages": True, "queries": cases,
        "resident_mib": {side: statistics.median(r["after_queries"]["resident_bytes"] for r in runs) / 2**20 for side, runs in records.items()},
        "restore_ms": {side: statistics.median(r["restore_ms"] for r in runs) for side, runs in records.items()},
        "records": records,
    }
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for side in ("baseline", "candidate"):
        parser.add_argument("--" + side + "-binary", type=Path, required=True)
        parser.add_argument("--" + side + "-cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--repetitions", type=int, default=25)
    parser.add_argument("--first-side", choices=("baseline", "candidate"), default="baseline")
    args = parser.parse_args()
    if args.rounds < 1 or args.repetitions < 1:
        parser.error("rounds and repetitions must be positive")
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    records = {"baseline": [], "candidate": []}
    for number in range(args.rounds):
        # AB / BA prevents always giving the candidate the second position.
        sides = ("baseline", "candidate") if (number + (args.first_side == "candidate")) % 2 == 0 else ("candidate", "baseline")
        for side in sides:
            binary = getattr(args, side + "_binary").resolve()
            cache = getattr(args, side + "_cache").resolve()
            environment = os.environ | {
                "APFSEARCH_ACCEPTANCE_CACHE": str(cache),
                "APFSEARCH_ACCEPTANCE_REPETITIONS": str(args.repetitions),
            }
            result = subprocess.run([
                str(binary), "--ignored", "--exact",
                "compact_acceptance_tests::same_corpus_resource_profile", "--nocapture",
            ], env=environment, text=True, capture_output=True, timeout=900)
            log = args.output / f"{number + 1}-{side}.log"
            log.write_text(result.stdout + result.stderr)
            if result.returncode:
                raise RuntimeError(f"Acceptance process failed; inspect {log}")
            record = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith('{"'))
            records[side].append(record)
            print(json.dumps({"round": number + 1, "side": side,
                              "entries": record["entries"], "restore_ms": record["restore_ms"],
                              "resident_mib": record["after_queries"]["resident_bytes"] / 2**20}), flush=True)
    summary = summarize_comparison(records, args.rounds, args.repetitions)
    (args.output / "comparison.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k != "records"}), flush=True)


if __name__ == "__main__":
    main()
