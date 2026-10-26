#!/usr/bin/env python3
"""Docker-controlled MyDB vs MySQL benchmark orchestrator.

Runs the `mydb-bench` workload against both databases (running under identical
Docker CPU/memory limits) and writes a reproducible Markdown report with
per-scenario medians and ratios. The performance phase deliberately has no
warmup and has a 60-second wall-clock budget.

Usage:
    python3 scripts/bench_docker.py [--samples N] [--memory 2g] [--cpus 2]
"""
from __future__ import annotations

import argparse
import atexit
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time
import urllib.request
import uuid

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_FILE = os.path.join(ROOT, "bench", "compose.yaml")
PROJECT = f"mydb-bench-{os.getpid()}-{uuid.uuid4().hex[:8]}"
MYDB_URL = "mysql://root:root@mydb:3306"
MYSQL_URL = "mysql://root:root@mysql:3306"
MYDB_HTTP_BASE = f"http://127.0.0.1:{os.environ.get('MYDB_HOST_HTTP_PORT', '14306')}"
MYDB_METRICS_URL = MYDB_HTTP_BASE + "/metrics"

SCENARIOS = [
    # name, human label, args
    ("single", "单表写（fsync-per-commit）",
     ["--actors", "1", "--table-count", "1", "--transaction-size", "1",
      "--writes-per-actor", "200", "--reads-per-actor", "0"]),
    ("concurrent-p99", "并发 P99（4 actor / 4 表，写+点查）",
     ["--actors", "4", "--table-count", "4", "--transaction-size", "5",
      "--writes-per-actor", "80", "--reads-per-actor", "10"]),
    ("concurrent-tp", "并发吞吐（4 actor / 4 表，单行事务）",
     ["--actors", "4", "--table-count", "4", "--transaction-size", "1",
      "--writes-per-actor", "200", "--reads-per-actor", "0"]),
]

COMMON = ["--reconnect-every-transactions", "0", "--payload-bytes", "256"]
PERFORMANCE_BUDGET_SECONDS = 60


def run(cmd, timeout=None):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def docker_compose(*args):
    cmd = ["docker", "compose", "-f", COMPOSE_FILE, "-p", PROJECT, *args]
    return run(cmd)


def container_id(service):
    result = docker_compose("ps", "--quiet", service)
    ids = result.stdout.split()
    if result.returncode != 0 or len(ids) != 1:
        sys.exit(f"cannot identify this run's {service} container")
    return ids[0]


def ensure_up(skip_build):
    if not skip_build:
        print("==> building Docker benchmark images", flush=True)
        r = docker_compose("build", "mydb", "bench")
        if r.returncode != 0:
            sys.exit(f"docker compose build failed:\n{r.stderr}")
    print("==> starting mydb + mysql", flush=True)
    r = docker_compose("up", "-d", "mydb", "mysql")
    if r.returncode != 0:
        sys.exit(f"docker compose up failed:\n{r.stderr}")
    # Wait for health.
    deadline = time.time() + 180
    for svc in ("mydb", "mysql"):
        while time.time() < deadline:
            inspect = run(["docker", "inspect", "-f",
                           "{{.State.Health.Status}}", container_id(svc)])
            status = inspect.stdout.strip()
            if status == "healthy":
                print(f"    {svc} healthy", flush=True)
                break
            time.sleep(2)
        else:
            sys.exit(f"{svc} did not become healthy in time")


def run_bench(url, args, timeout):
    cmd = ["docker", "compose", "-f", COMPOSE_FILE, "-p", PROJECT, "run",
           "--rm", "bench", "--url", url, *COMMON, *args]
    if timeout <= 0:
        sys.exit("performance phase exhausted its 60-second budget")
    try:
        r = run(cmd, timeout=timeout)
    except subprocess.TimeoutExpired:
        sys.exit("performance workload exceeded the remaining 60-second budget")
    if r.returncode != 0:
        sys.exit(f"bench failed for {url}:\n{r.stderr}\n{r.stdout}")
    text = r.stdout
    start = text.find("{")
    end = text.rfind("}")
    if start < 0 or end <= start:
        sys.exit(f"bench produced no JSON:\n{text}")
    return json.loads(text[start:end + 1])


def collect_metrics():
    try:
        login = urllib.request.Request(
            MYDB_HTTP_BASE + "/api/v1/auth/login",
            data=json.dumps({"username": "root", "password": "root"}).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(login, timeout=5) as response:
            token = json.load(response)["token"]
        request = urllib.request.Request(
            MYDB_METRICS_URL, headers={"Authorization": "Bearer " + token}
        )
        with urllib.request.urlopen(request, timeout=5) as response:
            metrics = response.read().decode()
        if "mydb_up 1\n" not in metrics:
            sys.exit("authenticated MyDB benchmark metrics are unavailable")
        return metrics
    except Exception as exc:  # noqa: BLE001
        sys.exit(f"authenticated MyDB benchmark metrics failed: {type(exc).__name__}")


def mysql_version():
    r = run([
        "docker", "exec", container_id("mysql"), "mysql", "-N", "-uroot", "-proot",
        "-e", "SELECT CONCAT(VERSION(), ' — ', @@version_comment)",
    ])
    if r.returncode != 0:
        sys.exit(f"could not read MySQL version:\n{r.stderr}")
    value = r.stdout.strip()
    if not value.startswith("8.4."):
        sys.exit(f"benchmark requires MySQL 8.4, got: {value}")
    return value


def metric(text, name):
    for line in text.splitlines():
        if line.startswith(name + " "):
            parts = line.split()
            if len(parts) == 2:
                return parts[1]
    return "n/a"


def median(values, key):
    return statistics.median([v[key] for v in values])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--samples", type=int, default=1)
    ap.add_argument("--memory", default=os.environ.get("MYDB_MEM", "2g"))
    ap.add_argument("--cpus", default=os.environ.get("MYDB_CPUS", "2"))
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    if not 1 <= args.samples <= 5:
        ap.error("--samples must be between 1 and 5")
    os.environ["MYDB_MEM"] = args.memory
    os.environ["MYSQL_MEM"] = args.memory
    os.environ["MYDB_CPUS"] = str(args.cpus)
    os.environ["MYSQL_CPUS"] = str(args.cpus)
    if not args.keep:
        atexit.register(lambda: docker_compose("down", "-v", "--remove-orphans"))
    ensure_up(args.skip_build)

    results = {"mydb": {}, "mysql": {}}
    raw_path = Path(ROOT) / "target" / ("benchmark-" + PROJECT) / "results.json"
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    commit = run(["git", "-C", ROOT, "rev-parse", "HEAD"]).stdout.strip()
    source_dirty = bool(run(["git", "-C", ROOT, "status", "--porcelain"]).stdout.strip())
    raw = {"format_version": 1, "source_revision": commit, "source_dirty": source_dirty,
           "samples": args.samples, "cpus_per_database": args.cpus, "memory_per_database": args.memory,
           "payload_bytes": 256, "warmup": False, "performance_budget_seconds": PERFORMANCE_BUDGET_SECONDS,
           "results": results, "complete": False}

    def save_raw():
        raw_path.write_text(json.dumps(raw, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    save_raw()
    performance_started = time.monotonic()
    for name, label, sc in SCENARIOS:
        print(f"==> scenario: {label}", flush=True)
        for engine, url in (("mydb", MYDB_URL), ("mysql", MYSQL_URL)):
            samples = results[engine][name] = []
            for _ in range(args.samples):
                remaining = PERFORMANCE_BUDGET_SECONDS - (time.monotonic() - performance_started)
                samples.append(run_bench(url, sc, remaining))
                raw["performance_elapsed_seconds"] = time.monotonic() - performance_started
                save_raw()
            med = median(samples, "operations_per_second")
            print(f"    {engine}: median {med:.0f} ops/s", flush=True)
            elapsed = time.monotonic() - performance_started
            if elapsed > PERFORMANCE_BUDGET_SECONDS:
                sys.exit(
                    f"performance phase exceeded {PERFORMANCE_BUDGET_SECONDS}s "
                    f"without warmup: {elapsed:.1f}s"
                )
    performance_elapsed = time.monotonic() - performance_started

    metrics = collect_metrics()
    mysql_build = mysql_version()
    date = time.strftime("%Y-%m-%d")
    daemon = json.loads(run(["docker", "info", "--format", "{{json .}}"]).stdout)
    mydb_id = container_id("mydb")
    mydb_build = run(["docker", "exec", mydb_id, "mydb-server", "--version"]).stdout.strip()
    mydb_binary_sha256 = run(["docker", "exec", mydb_id, "sha256sum", "/usr/local/bin/mydb-server"]).stdout.split()[0]
    raw.update({"complete": True, "performance_elapsed_seconds": performance_elapsed, "date": date,
                "docker_os": daemon.get("OperatingSystem"), "kernel": daemon.get("KernelVersion"),
                "architecture": daemon.get("Architecture"), "mydb_version": mydb_build,
                "mydb_binary_sha256": mydb_binary_sha256, "mysql_version": mysql_build})
    save_raw()
    public_raw = Path(ROOT) / "bench" / "results" / "latest.json"
    public_raw.parent.mkdir(parents=True, exist_ok=True)
    public_raw.write_text(raw_path.read_text(encoding="utf-8"), encoding="utf-8")

    def m(engine, name, key):
        return median(results[engine][name], key)

    single_m = m("mydb", "single", "operations_per_second")
    single_s = m("mysql", "single", "operations_per_second")
    p99_m = m("mydb", "concurrent-p99", "write_transactions_p99_us") / 1000.0
    p99_s = m("mysql", "concurrent-p99", "write_transactions_p99_us") / 1000.0
    tp_m = m("mydb", "concurrent-tp", "operations_per_second")
    tp_s = m("mysql", "concurrent-tp", "operations_per_second")
    read_m = m("mydb", "concurrent-p99", "reads_p50_us")
    read_s = m("mysql", "concurrent-p99", "reads_p50_us")

    single_ratio = single_m / single_s if single_s else 0
    p99_ratio = p99_m / p99_s if p99_s else 0
    tp_ratio = tp_m / tp_s if tp_s else 0

    lines = []
    lines.append("# MyDB 性能报告\n")
    lines.append(
        "> 本报告的对比在 **Docker 容器** 中进行，MyDB 与 MySQL 运行在**完全相同的 "
        f"CPU 与内存限制**下（各 {args.cpus} vCPU、{args.memory} 内存）。双方持久化级别"
        "一致：MyDB 每组一次 WAL `sync_data()`；MySQL `innodb_flush_log_at_trx_commit=1` "
        "+ `sync_binlog=1`。每项取 %d 次采样中位数。\n" % args.samples)
    lines.append("> 基准工具为 `mydb-bench`，双方执行完全相同的 `ENGINE=InnoDB` 业务 "
                 "DDL/DML；仅创建并删除唯一命名的 `mydb_game_bench_*` 临时库。性能阶段不预热，"
                 "默认单样本并要求在 60 秒内完成。\n")
    lines.append("---\n")
    lines.append("## 测试方法\n")
    lines.append("| 场景 | actors | tables | transaction_size | writes_per_actor | 说明 |")
    lines.append("|------|--------|--------|------------------|------------------|------|")
    lines.append("| 单表写 | 1 | 1 | 1 | 200 | 单连接单行事务，每次 COMMIT 持久化 |")
    lines.append("| 并发 P99 | 4 | 4 | 5 | 80 | 4 并发写、10 点查/actor |")
    lines.append("| 并发吞吐 | 4 | 4 | 1 | 200 | 800 次单行事务，测真实 group commit |\n")
    lines.append("## 当前实测（Docker 受控）\n")
    lines.append(f"### Git Revision: `{commit}`（source dirty: {str(source_dirty).lower()}）")
    lines.append(f"### 测试日期: {date}")
    lines.append(f"### 性能阶段耗时: {performance_elapsed:.1f}s / {PERFORMANCE_BUDGET_SECONDS}s（无预热）")
    lines.append(f"### 环境: {daemon.get('OperatingSystem')} / {daemon.get('Architecture')}，内核 `{daemon.get('KernelVersion')}`；双方 {args.cpus} vCPU / {args.memory}")
    lines.append(f"### MyDB 构建: {mydb_build}；SHA-256 `{mydb_binary_sha256}`")
    lines.append("### 原始样本: [bench/results/latest.json](bench/results/latest.json)；此容器宿主结果不等同物理 Ubuntu 平台验收")
    lines.append(f"### MySQL: {mysql_build} — `innodb_flush_log_at_trx_commit=1`, `sync_binlog=1`, `transaction_isolation=REPEATABLE-READ`")
    lines.append("### MyDB: `group_commit_window_us=250`，shard_count 自动探测分配 CPU，checkpoint 每 1024 个已提交请求\n")
    lines.append(f"| 场景 | MyDB | MySQL {mysql_build.split(' — ', 1)[0]} | MyDB / MySQL |")
    lines.append("|------|------|--------------|---------------|")
    lines.append(f"| 单表写 | {single_m:.0f} ops/s | {single_s:.0f} ops/s | {single_ratio:.2f}x |")
    lines.append(f"| 4 actor / 4表 写 P99 | {p99_m:.1f} ms | {p99_s:.1f} ms | {p99_ratio:.2f}x（MyDB/MySQL 延迟比，低更好） |")
    lines.append(f"| 4 actor / 4表 吞吐 | {tp_m:.0f} ops/s | {tp_s:.0f} ops/s | {tp_ratio:.2f}x |")
    lines.append(f"| 读 P50 | {read_m:.0f} μs | {read_s:.0f} μs | - |\n")
    lines.append("## MyDB 观测\n")
    lines.append(f"- write groups: {metric(metrics, 'mydb_group_commits_total')}")
    lines.append(f"- grouped requests: {metric(metrics, 'mydb_grouped_requests_total')}")
    lines.append(f"- checkpoints: {metric(metrics, 'mydb_checkpoints_total')}")
    lines.append(f"- WAL sync total: {metric(metrics, 'mydb_wal_sync_microseconds_total')} μs")
    lines.append(f"- checkpoint total: {metric(metrics, 'mydb_checkpoint_microseconds_total')} μs\n")
    lines.append("## 性能原则\n")
    lines.append("- 已确认提交必须由一次 `sync_data()` WAL durability boundary 覆盖；不以关闭持久化换吞吐。")
    lines.append("- 默认 250μs Group Commit 窗口优先并发吞吐；`0` 是显式低延迟档，报告必须写明所用档位。")
    lines.append("- 多核并行：MyDB 按表命名空间分片为独立 Leader/Follower 提交组，每组独立 WAL 与 fsync，"
                 "允许不同分片并行提交，扩展收益以实测为准。")
    lines.append("- checkpoint 按已提交请求数触发，不与合批大小耦合；崩溃恢复、`flush_consistent`、shutdown 仍强制落盘。")
    lines.append("- MySQL 比较必须同机（同容器宿主）、实际运行、相同负载，并记录 MySQL 版本与持久化设置；禁止硬编码历史比值。\n")

    report = "\n".join(lines)
    out = os.path.join(ROOT, "性能报告.md")
    with open(out, "w", encoding="utf-8") as fh:
        fh.write(report)
    print(f"\nReport written to {out}")
    print(f"single {single_ratio:.2f}x | p99 latency ratio {p99_ratio:.2f}x | tp {tp_ratio:.2f}x")


if __name__ == "__main__":
    main()
