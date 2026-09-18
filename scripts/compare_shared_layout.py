#!/usr/bin/env python3
"""Alternate immutable Rust acceptance binaries without scanning the filesystem.

Both binaries must contain the identical compact_acceptance_tests harness.
Output contains aggregate counters and fingerprints, never indexed file paths.
The output directory must be new; immutable inputs are never overwritten.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess


def percentile(samples, fraction=0.95):
    return sorted(samples)[math.ceil(len(samples) * fraction) - 1]


def within_latency_limit(baseline_ms, candidate_ms, complex_case=False):
    relative_limit = 1.15 if complex_case else 1.05
    return candidate_ms <= baseline_ms * relative_limit or (
        not complex_case and candidate_ms - baseline_ms <= 1
    )


def summarize_comparison(records, rounds, repetitions):
    fingerprints = {r["metadata_digest"] for runs in records.values() for r in runs}
    if len(fingerprints) != 1:
        raise AssertionError("Corpora do not match")
    cases = []
    for index in range(len(records["baseline"][0]["queries"])):
        rows = {side: [run["queries"][index] for run in runs] for side, runs in records.items()}
        identifiers = {row["case"] for runs in rows.values() for row in runs}
        if len(identifiers) != 1:
            raise AssertionError("Acceptance case sequences must agree")
        case = identifiers.pop()
        if len({row["digest"] for runs in rows.values() for row in runs}) != 1:
            raise AssertionError(f"Case {index} differs in result or order")
        limit = 15 if case in (14, 19, 20) else 5
        modes = {}
        for mode in ("uncached", "warm"):
            values = {side: percentile([v for row in runs for v in row[mode]["samples_ms"]]) for side, runs in rows.items()}
            delta = (values["candidate"] / values["baseline"] - 1) * 100
            increase_ms = values["candidate"] - values["baseline"]
            # Daily queries reject only an increase exceeding BOTH thresholds.
            # Complex regex/deep paging keep their independent 15% limit.
            passes = within_latency_limit(values["baseline"], values["candidate"], limit == 15)
            modes[mode] = values | {"regression_percent": delta, "increase_ms": increase_ms, "passes": passes}
        cases.append({"case": case, "allowed_regression_percent": limit, **modes})
    summary = {
        "rounds": rounds, "repetitions_per_mode_per_round": repetitions,
        "scope": "Core restore and query; excludes XPC, UI and event reconciliation",
        "same_corpus_and_pages": True, "queries": cases,
        "resident_mib": {side: statistics.median(r["after_queries"]["resident_bytes"] for r in runs) / 2**20 for side, runs in records.items()},
        "restore_ms": {side: statistics.median(r["restore_ms"] for r in runs) for side, runs in records.items()},
        "resource_medians": {side: {
            phase: {field: statistics.median(run[phase][field] for run in runs)
                    for field in ("resident_bytes", "physical_footprint_bytes", "peak_physical_footprint_bytes", "pageins", "minor_faults", "major_faults", "cpu_seconds", "disk_bytes_read", "disk_bytes_written", "logical_writes")}
            for phase in ("restored", "after_queries")
        } for side, runs in records.items()},
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
    parser.add_argument("--cases", help="Comma-separated case IDs for a labelled diagnostic subset; omit for full acceptance")
    args = parser.parse_args()
    if args.rounds < 1 or args.repetitions < 1:
        parser.error("rounds and repetitions must be positive")
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    records = {"baseline": [], "candidate": []}
    binary_sha256 = {side: hashlib.sha256(getattr(args, side + "_binary").read_bytes()).hexdigest()
                     for side in records}
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
            environment.pop("APFSEARCH_ACCEPTANCE_CASES", None)
            if args.cases:
                environment["APFSEARCH_ACCEPTANCE_CASES"] = args.cases
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
    summary["case_selection"] = args.cases or "all"
    summary["binary_sha256"] = binary_sha256
    (args.output / "comparison.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k != "records"}), flush=True)


if __name__ == "__main__":
    main()
