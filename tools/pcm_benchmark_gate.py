#!/usr/bin/env python3
"""Plan and enforce fresh-runner confirmations for the PCM benchmark.

The initial benchmark job records every timing and memory check.  Timing
checks which exceed their unchanged limit are converted into a minimal set of
confirmation populations.  A second GitHub-hosted runner then measures only
those populations and may reject the change only when the same statistic
exceeds the same limit on both runners.  Memory checks remain initial-runner
gates because repeating a peak-RSS regression would weaken their contract.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any, Callable

import paired_benchmark


PLAN_SCHEMA = "forge-pcm-fresh-confirmation-plan/v1"
EVIDENCE_SCHEMA = "forge-pcm-fresh-confirmation-evidence/v1"
LENGTHS = {"short": 300, "long": 600}
MODES = ("default", "one")
CASES = paired_benchmark.CASES
ROUNDS = 8
SAMPLES_PER_BINARY = 16
RSS_LIMIT_BYTES = 4 * 1024 * 1024
GROSS_RSS_LIMIT_BYTES = 132 * 1024 * 1024
GROSS_RSS_SAMPLE_LIMIT = 1


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def _finite_number(value: Any, description: str) -> float:
    _require(
        isinstance(value, (int, float)) and not isinstance(value, bool),
        f"{description} is not numeric",
    )
    number = float(value)
    _require(math.isfinite(number), f"{description} is non-finite")
    return number


def _read_json(path: Path) -> dict[str, Any]:
    document = json.loads(path.read_text(encoding="utf-8"))
    _require(isinstance(document, dict), f"{path}: document is not an object")
    return document


def _write_json(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def package_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as source:
        version = tomllib.load(source)["package"]["version"]
    _require(isinstance(version, str) and version, f"invalid package version in {root}")
    return version


def measurement_revision(root: Path) -> str:
    prefix = 'pub const MEASUREMENT_ALGORITHM_REVISION: &str = "'
    for line in (root / "src/bound_analysis.rs").read_text(
        encoding="utf-8"
    ).splitlines():
        if line.startswith(prefix) and line.endswith('";'):
            return line[len(prefix) : -2]
    raise ValueError(f"missing measurement revision in {root}")


def _validate_sha(value: str, description: str) -> str:
    _require(
        isinstance(value, str)
        and len(value) == 40
        and all(character in "0123456789abcdef" for character in value),
        f"{description} is not a full lowercase Git SHA",
    )
    return value


def _expected_platform(runner_os: str, runner_arch: str) -> tuple[str, set[str]]:
    platforms = {
        ("Linux", "X64"): ("Linux", {"x86_64", "amd64"}),
        ("macOS", "ARM64"): ("Darwin", {"arm64", "aarch64"}),
    }
    _require(
        (runner_os, runner_arch) in platforms,
        f"unsupported runner platform: {runner_os}/{runner_arch}",
    )
    return platforms[(runner_os, runner_arch)]


def threshold_context(baseline_root: Path, candidate_root: Path) -> dict[str, Any]:
    baseline_version = package_version(baseline_root)
    candidate_version = package_version(candidate_root)
    baseline_revision = measurement_revision(baseline_root)
    candidate_revision = measurement_revision(candidate_root)
    measurement_migration = (
        baseline_revision == "forge-bs1770-5-r3"
        and candidate_revision == "forge-bs1770-5-r4"
    )
    _require(
        baseline_revision == candidate_revision or measurement_migration,
        "unapproved measurement revision transition",
    )
    durability_migration = (
        baseline_version == "0.189.5" and candidate_version == "0.189.6"
    )
    return {
        "baseline_version": baseline_version,
        "candidate_version": candidate_version,
        "baseline_measurement_revision": baseline_revision,
        "candidate_measurement_revision": candidate_revision,
        "measurement_migration": measurement_migration,
        "durability_migration": durability_migration,
        "limits_percent": {
            "short_control_cpu": (
                8.0
                if measurement_migration
                else 5.0 if durability_migration else 4.0
            ),
            "short_resample_wall": (
                8.0
                if measurement_migration
                else 5.0 if durability_migration else 4.0
            ),
            "short_resample_cpu": (
                8.0
                if measurement_migration
                else 4.0 if durability_migration else 3.0
            ),
            "short_average": (
                5.0
                if measurement_migration
                else 2.0 if durability_migration else 1.0
            ),
            "short_paired_wall": 10.0,
            "short_paired_cpu": 8.0,
            "short_pooled": 20.0,
            "long_isolated": (
                8.0 if measurement_migration or durability_migration else 5.0
            ),
            "long_average": 5.0 if measurement_migration else 3.0,
            "long_pooled": 20.0 if measurement_migration else 15.0,
        },
    }


def _validate_sample(sample: Any, description: str) -> None:
    _require(isinstance(sample, dict), f"{description} is not an object")
    expected_keys = {
        "exit_code",
        "wall_seconds",
        "user_cpu_seconds",
        "system_cpu_seconds",
        "cpu_percent",
        "peak_rss_bytes",
    }
    _require(
        set(sample) == expected_keys,
        f"{description} has unexpected measurement fields",
    )
    wall = _finite_number(sample.get("wall_seconds"), f"{description} wall")
    user = _finite_number(sample.get("user_cpu_seconds"), f"{description} user CPU")
    system = _finite_number(
        sample.get("system_cpu_seconds"), f"{description} system CPU"
    )
    cpu_percent = _finite_number(
        sample.get("cpu_percent"), f"{description} CPU percent"
    )
    exit_code = sample.get("exit_code")
    _require(
        isinstance(exit_code, int) and not isinstance(exit_code, bool),
        f"{description} exit code is not an integer",
    )
    rss_value = sample.get("peak_rss_bytes")
    _require(
        isinstance(rss_value, int) and not isinstance(rss_value, bool),
        f"{description} peak RSS is not an integer",
    )
    rss = _finite_number(rss_value, f"{description} peak RSS")
    _require(wall > 0.0, f"{description} wall must be positive")
    _require(user >= 0.0 and system >= 0.0, f"{description} CPU must be non-negative")
    _require(cpu_percent >= 0.0, f"{description} CPU percent must be non-negative")
    _require(rss >= 0.0, f"{description} peak RSS must be non-negative")
    _require(exit_code == 0, f"{description} did not exit successfully")


def validate_report(
    path: Path,
    *,
    expected_duration: int,
    expected_cases: list[str],
    runner_os: str,
    runner_arch: str,
    baseline_version: str,
    candidate_version: str,
) -> dict[str, Any]:
    document = _read_json(path)
    _require(
        document.get("generator") == "forge-paired-benchmark/1",
        f"{path}: unexpected report generator",
    )
    _require(document.get("passed") is True, f"{path}: benchmark did not pass")
    _require(document.get("error") is None, f"{path}: benchmark recorded an error")
    expected_configuration = {
        "duration_seconds": expected_duration,
        "warmup_rounds": paired_benchmark.WARMUP_ROUNDS,
        "rounds": ROUNDS,
        "samples_per_binary": SAMPLES_PER_BINARY,
        "cases": expected_cases,
    }
    _require(
        document.get("configuration") == expected_configuration,
        f"{path}: unexpected configuration",
    )
    _require(
        document.get("versions")
        == {
            "baseline": f"forge {baseline_version}",
            "candidate": f"forge {candidate_version}",
        },
        f"{path}: unexpected binary versions",
    )
    expected_system, expected_architectures = _expected_platform(
        runner_os, runner_arch
    )
    system = document.get("system")
    _require(isinstance(system, dict), f"{path}: missing system evidence")
    _require(system.get("os") == expected_system, f"{path}: runner OS mismatch")
    _require(
        system.get("architecture") in expected_architectures,
        f"{path}: runner architecture mismatch",
    )
    results = document.get("results")
    _require(isinstance(results, list), f"{path}: results are not an array")
    _require(
        [result.get("id") for result in results if isinstance(result, dict)]
        == expected_cases,
        f"{path}: unexpected result cases",
    )
    for result in results:
        case_id = result["id"]
        _require(
            result.get("samples_per_binary") == SAMPLES_PER_BINARY,
            f"{path}: {case_id}: unexpected sample count",
        )
        expected_schedule = paired_benchmark.alternating_schedule(
            ROUNDS, inverted=CASES.index(case_id) % 2 == 1
        )
        _require(
            result.get("schedule") == expected_schedule,
            f"{path}: {case_id}: unbalanced or unexpected schedule",
        )
        for label in (paired_benchmark.BASELINE, paired_benchmark.CANDIDATE):
            samples = result.get(f"{label}_samples")
            _require(
                isinstance(samples, list) and len(samples) == SAMPLES_PER_BINARY,
                f"{path}: {case_id}: incomplete {label} population",
            )
            for sample_index, sample in enumerate(samples):
                _validate_sample(sample, f"{path}: {case_id}: {label}[{sample_index}]")
        # Exercise both reducers here so malformed populations cannot be hidden
        # until the final gate.
        for getter in (
            lambda sample: sample["wall_seconds"],
            lambda sample: sample["user_cpu_seconds"]
            + sample["system_cpu_seconds"],
        ):
            paired_benchmark.paired_median_change_percent(result, getter)
            paired_benchmark.pooled_median_change_percent(result, getter)
    return document


def _result_map(document: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {result["id"]: result for result in document["results"]}


def _getter(metric: str) -> Callable[[dict[str, Any]], float]:
    if metric == "wall":
        return lambda sample: sample["wall_seconds"]
    if metric == "cpu":
        return lambda sample: sample["user_cpu_seconds"] + sample["system_cpu_seconds"]
    raise ValueError(f"unsupported timing metric: {metric}")


def _change(result: dict[str, Any], statistic: str, metric: str) -> float:
    getter = _getter(metric)
    if statistic == "paired":
        return paired_benchmark.paired_median_change_percent(result, getter)
    if statistic == "pooled":
        return paired_benchmark.pooled_median_change_percent(result, getter)
    raise ValueError(f"unsupported timing statistic: {statistic}")


def _report_name(length: str, mode: str) -> str:
    return f"{length}-{mode}.json"


def load_initial_results(
    input_dir: Path,
    *,
    runner_os: str,
    runner_arch: str,
    context: dict[str, Any],
) -> tuple[dict[tuple[str, str, str], dict[str, Any]], list[dict[str, Any]]]:
    results = {}
    reports = []
    for length, duration in LENGTHS.items():
        for mode in MODES:
            name = _report_name(length, mode)
            path = input_dir / name
            document = validate_report(
                path,
                expected_duration=duration,
                expected_cases=list(CASES),
                runner_os=runner_os,
                runner_arch=runner_arch,
                baseline_version=context["baseline_version"],
                candidate_version=context["candidate_version"],
            )
            for case, result in _result_map(document).items():
                results[(length, mode, case)] = result
            reports.append(
                {
                    "length": length,
                    "mode": mode,
                    "file": name,
                    "sha256": file_sha256(path),
                }
            )
    _require(
        len(results) == len(LENGTHS) * len(MODES) * len(CASES),
        "initial benchmark population is incomplete",
    )
    return results, reports


def _timing_check(
    *,
    scope: str,
    length: str,
    mode: str,
    case: str,
    statistic: str,
    metric: str,
    value: float,
    limit: float,
) -> dict[str, Any]:
    _finite_number(value, "timing change")
    _finite_number(limit, "timing limit")
    identifier = "/".join(
        (length, scope, mode, case, statistic, metric)
    )
    exceeded = value > limit
    return {
        "id": identifier,
        "scope": scope,
        "length": length,
        "duration_seconds": LENGTHS[length],
        "mode": mode,
        "case": case,
        "statistic": statistic,
        "metric": metric,
        "initial_change_percent": value,
        "limit_percent": limit,
        "exceeded": exceeded,
        "confirmation_requested": exceeded,
    }


def timing_checks(
    results: dict[tuple[str, str, str], dict[str, Any]],
    limits: dict[str, float],
) -> list[dict[str, Any]]:
    checks = []

    short_paired: dict[tuple[str, str, str], float] = {}
    for mode in MODES:
        for case in CASES:
            result = results[("short", mode, case)]
            for metric in ("wall", "cpu"):
                value = _change(result, "paired", metric)
                limit = limits[f"short_paired_{metric}"]
                if case == "flac-stereo-normalize" and metric == "cpu":
                    limit = min(limit, limits["short_control_cpu"])
                if mode == "one" and case == "wav-stereo-resample-normalize":
                    if metric == "wall":
                        limit = min(limit, limits["short_resample_wall"])
                    else:
                        limit = min(limit, limits["short_resample_cpu"])
                short_paired[(mode, case, metric)] = value
                checks.append(
                    _timing_check(
                        scope="case",
                        length="short",
                        mode=mode,
                        case=case,
                        statistic="paired",
                        metric=metric,
                        value=value,
                        limit=limit,
                    )
                )
                checks.append(
                    _timing_check(
                        scope="case",
                        length="short",
                        mode=mode,
                        case=case,
                        statistic="pooled",
                        metric=metric,
                        value=_change(result, "pooled", metric),
                        limit=limits["short_pooled"],
                    )
                )
    for metric in ("wall", "cpu"):
        value = statistics.fmean(
            change
            for (mode, case, candidate_metric), change in short_paired.items()
            if candidate_metric == metric
        )
        checks.append(
            _timing_check(
                scope="aggregate",
                length="short",
                mode="all",
                case="all",
                statistic="paired",
                metric=metric,
                value=value,
                limit=limits["short_average"],
            )
        )

    long_paired = []
    for mode in MODES:
        for case in CASES:
            result = results[("long", mode, case)]
            for metric in ("wall", "cpu"):
                paired = _change(result, "paired", metric)
                long_paired.append(paired)
                checks.append(
                    _timing_check(
                        scope="case",
                        length="long",
                        mode=mode,
                        case=case,
                        statistic="paired",
                        metric=metric,
                        value=paired,
                        limit=limits["long_isolated"],
                    )
                )
                checks.append(
                    _timing_check(
                        scope="case",
                        length="long",
                        mode=mode,
                        case=case,
                        statistic="pooled",
                        metric=metric,
                        value=_change(result, "pooled", metric),
                        limit=limits["long_pooled"],
                    )
                )
    checks.append(
        _timing_check(
            scope="aggregate",
            length="long",
            mode="all",
            case="all",
            statistic="paired",
            metric="wall_cpu",
            value=statistics.fmean(long_paired),
            limit=limits["long_average"],
        )
    )
    return checks


def memory_checks(
    results: dict[tuple[str, str, str], dict[str, Any]],
) -> list[dict[str, Any]]:
    checks = []
    for length in LENGTHS:
        for mode in MODES:
            for case in CASES:
                result = results[(length, mode, case)]
                baseline = [
                    sample["peak_rss_bytes"] for sample in result["baseline_samples"]
                ]
                candidate = [
                    sample["peak_rss_bytes"] for sample in result["candidate_samples"]
                ]
                baseline_median = statistics.median(baseline)
                delta = statistics.median(candidate) - baseline_median
                checks.append(
                    {
                        "id": f"{length}/{mode}/{case}/median_rss",
                        "length": length,
                        "mode": mode,
                        "case": case,
                        "metric": "median_rss_delta_bytes",
                        "value": delta,
                        "limit": RSS_LIMIT_BYTES,
                        "passed": delta <= RSS_LIMIT_BYTES,
                    }
                )
                if length == "long":
                    gross = sum(
                        value > baseline_median + GROSS_RSS_LIMIT_BYTES
                        for value in candidate
                    )
                    checks.append(
                        {
                            "id": f"{length}/{mode}/{case}/gross_rss_samples",
                            "length": length,
                            "mode": mode,
                            "case": case,
                            "metric": "gross_rss_samples",
                            "value": gross,
                            "limit": GROSS_RSS_SAMPLE_LIMIT,
                            "gross_delta_bytes": GROSS_RSS_LIMIT_BYTES,
                            "passed": gross <= GROSS_RSS_SAMPLE_LIMIT,
                        }
                    )
    return checks


def confirmation_populations(detectors: list[dict[str, Any]]) -> list[dict[str, Any]]:
    requested: set[tuple[str, str, str]] = set()
    aggregate_lengths = {
        detector["length"]
        for detector in detectors
        if detector["scope"] == "aggregate"
    }
    for length in aggregate_lengths:
        requested.update((length, mode, case) for mode in MODES for case in CASES)
    for detector in detectors:
        if detector["scope"] == "case":
            requested.add(
                (detector["length"], detector["mode"], detector["case"])
            )

    populations = []
    for length, duration in LENGTHS.items():
        for mode in MODES:
            cases = [case for case in CASES if (length, mode, case) in requested]
            if cases:
                populations.append(
                    {
                        "id": f"{length}/{mode}",
                        "length": length,
                        "duration_seconds": duration,
                        "mode": mode,
                        "cases": cases,
                        "rounds": ROUNDS,
                        "samples_per_binary": SAMPLES_PER_BINARY,
                        "output": f"fresh-confirmation-{length}-{mode}.json",
                    }
                )
    return populations


def build_plan(
    input_dir: Path,
    *,
    baseline_root: Path,
    candidate_root: Path,
    base_sha: str,
    head_sha: str,
    runner_os: str,
    runner_arch: str,
) -> dict[str, Any]:
    base_sha = _validate_sha(base_sha, "base SHA")
    head_sha = _validate_sha(head_sha, "head SHA")
    _expected_platform(runner_os, runner_arch)
    context = threshold_context(baseline_root, candidate_root)
    results, reports = load_initial_results(
        input_dir,
        runner_os=runner_os,
        runner_arch=runner_arch,
        context=context,
    )
    checks = timing_checks(results, context["limits_percent"])
    detectors = [dict(check) for check in checks if check["exceeded"]]
    rss_checks = memory_checks(results)
    populations = confirmation_populations(detectors)
    return {
        "schema": PLAN_SCHEMA,
        "source": {"base_sha": base_sha, "head_sha": head_sha},
        "initial_runner": {"os": runner_os, "arch": runner_arch},
        "measurement_context": context,
        "configuration": {
            "durations_seconds": LENGTHS,
            "warmup_rounds": paired_benchmark.WARMUP_ROUNDS,
            "rounds": ROUNDS,
            "samples_per_binary": SAMPLES_PER_BINARY,
            "modes": list(MODES),
            "cases": list(CASES),
        },
        "initial_reports": reports,
        "timing_checks": checks,
        "detectors": detectors,
        "confirmation_populations": populations,
        "confirmation_required": bool(detectors),
        "memory_checks": rss_checks,
        "initial_memory_passed": all(check["passed"] for check in rss_checks),
    }


def validate_plan(
    plan_path: Path,
    input_dir: Path,
    **expected: Any,
) -> dict[str, Any]:
    plan = _read_json(plan_path)
    _require(plan.get("schema") == PLAN_SCHEMA, "unexpected confirmation plan schema")
    rebuilt = build_plan(input_dir, **expected)
    _require(plan == rebuilt, "confirmation plan does not match bound initial evidence")
    return plan


def _confirmation_result_map(
    plan: dict[str, Any],
    confirmation_dir: Path,
    *,
    runner_os: str,
    runner_arch: str,
) -> tuple[
    dict[tuple[str, str, str], dict[str, Any]], list[dict[str, Any]]
]:
    context = plan["measurement_context"]
    results = {}
    reports = []
    for population in plan["confirmation_populations"]:
        path = confirmation_dir / population["output"]
        document = validate_report(
            path,
            expected_duration=population["duration_seconds"],
            expected_cases=population["cases"],
            runner_os=runner_os,
            runner_arch=runner_arch,
            baseline_version=context["baseline_version"],
            candidate_version=context["candidate_version"],
        )
        for case, result in _result_map(document).items():
            results[(population["length"], population["mode"], case)] = result
        reports.append(
            {
                "population_id": population["id"],
                "file": population["output"],
                "sha256": file_sha256(path),
            }
        )
    expected_count = sum(
        len(population["cases"])
        for population in plan["confirmation_populations"]
    )
    _require(len(results) == expected_count, "confirmation population is incomplete")
    return results, reports


def _confirmation_change(
    detector: dict[str, Any],
    results: dict[tuple[str, str, str], dict[str, Any]],
) -> float:
    if detector["scope"] == "case":
        result = results[
            (detector["length"], detector["mode"], detector["case"])
        ]
        return _change(result, detector["statistic"], detector["metric"])
    if detector["scope"] == "aggregate" and detector["length"] == "short":
        return statistics.fmean(
            _change(results[("short", mode, case)], "paired", detector["metric"])
            for mode in MODES
            for case in CASES
        )
    if detector["scope"] == "aggregate" and detector["length"] == "long":
        _require(
            detector["metric"] == "wall_cpu",
            "unexpected long aggregate metric",
        )
        return statistics.fmean(
            _change(results[("long", mode, case)], "paired", metric)
            for mode in MODES
            for case in CASES
            for metric in ("wall", "cpu")
        )
    raise ValueError(f"unsupported detector scope: {detector}")


def build_confirmation_evidence(
    plan: dict[str, Any],
    plan_path: Path,
    confirmation_dir: Path,
    *,
    runner_os: str,
    runner_arch: str,
) -> dict[str, Any]:
    _expected_platform(runner_os, runner_arch)
    results, reports = _confirmation_result_map(
        plan,
        confirmation_dir,
        runner_os=runner_os,
        runner_arch=runner_arch,
    )
    detector_evidence = []
    for detector in plan["detectors"]:
        confirmation = _confirmation_change(detector, results)
        _finite_number(confirmation, f"{detector['id']} confirmation change")
        entry = dict(detector)
        entry["confirmation_change_percent"] = confirmation
        entry["reproduced"] = paired_benchmark.regression_reproduced(
            detector["initial_change_percent"],
            confirmation,
            detector["limit_percent"],
        )
        detector_evidence.append(entry)
    return {
        "schema": EVIDENCE_SCHEMA,
        "plan_sha256": file_sha256(plan_path),
        "source": plan["source"],
        "initial_runner": plan["initial_runner"],
        "fresh_runner": {"os": runner_os, "arch": runner_arch},
        "requested": plan["confirmation_required"],
        "reports": reports,
        "detectors": detector_evidence,
        "passed": not any(entry["reproduced"] for entry in detector_evidence),
        "error": None,
    }


def enforce_initial_memory(plan: dict[str, Any]) -> None:
    """Reject any initial-runner RSS check which exceeds its fixed budget."""

    failures = [check for check in plan["memory_checks"] if not check["passed"]]
    _require(plan["initial_memory_passed"] == (not failures), "inconsistent RSS plan")
    if failures:
        raise RuntimeError(f"initial RSS regression: {failures}")


def validate_confirmation_evidence(
    plan: dict[str, Any],
    plan_path: Path,
    confirmation_dir: Path,
    evidence_path: Path,
    *,
    runner_os: str,
    runner_arch: str,
) -> dict[str, Any]:
    """Bind summarized evidence back to its plan and complete raw reports."""

    evidence = _read_json(evidence_path)
    _require(
        evidence.get("schema") == EVIDENCE_SCHEMA,
        "unexpected fresh confirmation evidence schema",
    )
    expected = build_confirmation_evidence(
        plan,
        plan_path,
        confirmation_dir,
        runner_os=runner_os,
        runner_arch=runner_arch,
    )
    _require(evidence == expected, "fresh evidence does not match raw reports")
    return evidence


def enforce_fresh_evidence(
    plan: dict[str, Any], evidence: dict[str, Any]
) -> None:
    """Apply initial RSS and same-statistic fresh-runner timing gates."""

    enforce_initial_memory(plan)
    failures = [entry for entry in evidence["detectors"] if entry["reproduced"]]
    _require(
        evidence.get("passed") == (not failures),
        "inconsistent fresh timing evidence",
    )
    if failures:
        raise RuntimeError(f"timing regression reproduced on fresh runner: {failures}")


def write_initial_markdown(plan: dict[str, Any], path: Path) -> None:
    lines = [
        "## PCM spool benchmark: initial runner",
        "",
        f"Base: `{plan['source']['base_sha']}`  ",
        f"Head: `{plan['source']['head_sha']}`  ",
        f"Runner: `{plan['initial_runner']['os']}/{plan['initial_runner']['arch']}`",
        "",
        "| Length | Scope | Mode | Case | Statistic | Metric | Initial | Limit |",
        "|---|---|---|---|---|---|---:|---:|",
    ]
    for check in plan["timing_checks"]:
        marker = " **(confirmation requested)**" if check["exceeded"] else ""
        lines.append(
            f"| {check['length']} | {check['scope']} | {check['mode']} "
            f"| {check['case']} | {check['statistic']} | {check['metric']} "
            f"| {check['initial_change_percent']:+.3f}%{marker} "
            f"| {check['limit_percent']:.3f}% |"
        )
    lines.extend(
        [
            "",
            f"Fresh confirmation required: `{str(plan['confirmation_required']).lower()}`",
            f"Initial RSS gates passed: `{str(plan['initial_memory_passed']).lower()}`",
            "",
        ]
    )
    path.write_text("\n".join(lines), encoding="utf-8")


def write_confirmation_markdown(evidence: dict[str, Any], path: Path) -> None:
    lines = [
        "## PCM spool benchmark: fresh runner confirmation",
        "",
        f"Fresh runner: `{evidence['fresh_runner']['os']}/{evidence['fresh_runner']['arch']}`",
        "",
    ]
    if not evidence["requested"]:
        lines.extend(["No timing confirmation was requested.", ""])
    else:
        lines.extend(
            [
                "| Length | Scope | Mode | Case | Statistic | Metric | Initial "
                "| Fresh | Limit | Reproduced |",
                "|---|---|---|---|---|---|---:|---:|---:|---|",
            ]
        )
        for entry in evidence["detectors"]:
            lines.append(
                f"| {entry['length']} | {entry['scope']} | {entry['mode']} "
                f"| {entry['case']} | {entry['statistic']} | {entry['metric']} "
                f"| {entry['initial_change_percent']:+.3f}% "
                f"| {entry['confirmation_change_percent']:+.3f}% "
                f"| {entry['limit_percent']:.3f}% "
                f"| {str(entry['reproduced']).lower()} |"
            )
        lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def run_confirmations(
    plan: dict[str, Any],
    *,
    paired_script: Path,
    baseline_forge: Path,
    candidate_forge: Path,
    ffmpeg: Path,
    work_dir: Path,
    confirmation_dir: Path,
    timeout_seconds: int = 7200,
) -> None:
    if not plan["confirmation_required"]:
        return
    for description, path in (
        ("paired benchmark script", paired_script),
        ("baseline forge", baseline_forge),
        ("candidate forge", candidate_forge),
        ("ffmpeg", ffmpeg),
    ):
        _require(path.is_file(), f"{description} is unavailable: {path}")
    confirmation_dir.mkdir(parents=True, exist_ok=True)
    work_dir.mkdir(parents=True, exist_ok=True)
    for population in plan["confirmation_populations"]:
        output = confirmation_dir / population["output"]
        output.unlink(missing_ok=True)
        command = [
            sys.executable,
            str(paired_script),
            "--baseline-forge",
            str(baseline_forge),
            "--candidate-forge",
            str(candidate_forge),
            "--ffmpeg",
            str(ffmpeg),
            "--duration-seconds",
            str(population["duration_seconds"]),
            "--rounds",
            str(ROUNDS),
            "--timeout-seconds",
            str(timeout_seconds),
            "--work-dir",
            str(work_dir / population["length"] / population["mode"]),
            "--output",
            str(output),
        ]
        for case in population["cases"]:
            command.extend(("--case", case))
        environment = os.environ.copy()
        if population["mode"] == "one":
            environment["RAYON_NUM_THREADS"] = "1"
        else:
            environment.pop("RAYON_NUM_THREADS", None)
        subprocess.run(command, check=True, env=environment)


def _add_bound_arguments(
    parser: argparse.ArgumentParser, *, include_plan: bool = True
) -> None:
    if include_plan:
        parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--initial-dir", type=Path, required=True)
    parser.add_argument("--baseline-root", type=Path, required=True)
    parser.add_argument("--candidate-root", type=Path, required=True)
    parser.add_argument("--base-sha", required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--runner-os", required=True)
    parser.add_argument("--runner-arch", required=True)


def _bound_arguments(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "baseline_root": args.baseline_root,
        "candidate_root": args.candidate_root,
        "base_sha": args.base_sha,
        "head_sha": args.head_sha,
        "runner_os": args.runner_os,
        "runner_arch": args.runner_arch,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    plan = subparsers.add_parser("plan")
    _add_bound_arguments(plan, include_plan=False)
    plan.add_argument("--output", type=Path, required=True)
    plan.add_argument("--summary-json", type=Path, required=True)
    plan.add_argument("--summary-markdown", type=Path, required=True)

    validate = subparsers.add_parser("validate-plan")
    _add_bound_arguments(validate)
    validate.add_argument("--github-output", type=Path)

    initial = subparsers.add_parser("enforce-initial")
    _add_bound_arguments(initial)

    confirm = subparsers.add_parser("run-confirmations")
    _add_bound_arguments(confirm)
    confirm.add_argument("--paired-script", type=Path, required=True)
    confirm.add_argument("--baseline-forge", type=Path, required=True)
    confirm.add_argument("--candidate-forge", type=Path, required=True)
    confirm.add_argument("--ffmpeg", type=Path, required=True)
    confirm.add_argument("--work-dir", type=Path, required=True)
    confirm.add_argument("--confirmation-dir", type=Path, required=True)
    confirm.add_argument("--evidence-output", type=Path, required=True)
    confirm.add_argument("--summary-markdown", type=Path, required=True)
    confirm.add_argument("--timeout-seconds", type=int, default=7200)

    enforce = subparsers.add_parser("enforce-fresh")
    _add_bound_arguments(enforce)
    enforce.add_argument("--confirmation-dir", type=Path, required=True)
    enforce.add_argument("--evidence", type=Path, required=True)
    return parser.parse_args()


def _validated_plan(args: argparse.Namespace) -> dict[str, Any]:
    _require(args.plan is not None, "--plan is required")
    return validate_plan(args.plan, args.initial_dir, **_bound_arguments(args))


def main() -> int:
    args = parse_args()
    if args.command == "plan":
        document = build_plan(args.initial_dir, **_bound_arguments(args))
        _write_json(args.output, document)
        _write_json(args.summary_json, document)
        write_initial_markdown(document, args.summary_markdown)
        print(json.dumps({
            "confirmation_required": document["confirmation_required"],
            "initial_memory_passed": document["initial_memory_passed"],
            "detectors": len(document["detectors"]),
        }, sort_keys=True))
        return 0
    if args.command == "validate-plan":
        document = _validated_plan(args)
        required = str(document["confirmation_required"]).lower()
        if args.github_output is not None:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"required={required}\n")
        print(f"confirmation_required={required}")
        return 0
    if args.command == "enforce-initial":
        document = _validated_plan(args)
        enforce_initial_memory(document)
        print("initial RSS gates passed")
        return 0
    if args.command == "run-confirmations":
        document = _validated_plan(args)
        try:
            run_confirmations(
                document,
                paired_script=args.paired_script,
                baseline_forge=args.baseline_forge,
                candidate_forge=args.candidate_forge,
                ffmpeg=args.ffmpeg,
                work_dir=args.work_dir,
                confirmation_dir=args.confirmation_dir,
                timeout_seconds=args.timeout_seconds,
            )
            evidence = build_confirmation_evidence(
                document,
                args.plan,
                args.confirmation_dir,
                runner_os=args.runner_os,
                runner_arch=args.runner_arch,
            )
        except Exception as error:
            evidence = {
                "schema": EVIDENCE_SCHEMA,
                "plan_sha256": file_sha256(args.plan),
                "source": document["source"],
                "initial_runner": document["initial_runner"],
                "fresh_runner": {
                    "os": args.runner_os,
                    "arch": args.runner_arch,
                },
                "requested": document["confirmation_required"],
                "reports": [],
                "detectors": [],
                "passed": False,
                "error": str(error),
            }
            _write_json(args.evidence_output, evidence)
            write_confirmation_markdown(evidence, args.summary_markdown)
            raise
        _write_json(args.evidence_output, evidence)
        write_confirmation_markdown(evidence, args.summary_markdown)
        print(json.dumps(evidence, indent=2, sort_keys=True))
        return 0
    if args.command == "enforce-fresh":
        document = _validated_plan(args)
        evidence = validate_confirmation_evidence(
            document,
            args.plan,
            args.confirmation_dir,
            args.evidence,
            runner_os=args.runner_os,
            runner_arch=args.runner_arch,
        )
        enforce_fresh_evidence(document, evidence)
        print("fresh-runner timing gates passed")
        return 0
    raise AssertionError(args.command)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"pcm-benchmark-gate: error: {error}", file=sys.stderr)
        raise SystemExit(2)
