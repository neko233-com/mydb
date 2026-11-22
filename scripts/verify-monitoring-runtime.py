#!/usr/bin/env python3
"""Bounded native release smoke; standard library only, no production data."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-directory", required=True, type=Path)
    parser.add_argument("--expected-version", required=True)
    args = parser.parse_args()
    suffix = ".exe" if os.name == "nt" else ""
    server = (args.binary_directory / ("mydb-server" + suffix)).resolve(strict=True)
    client = (args.binary_directory / ("mydb-cli" + suffix)).resolve(strict=True)
    sql_port, http_port = port(), port()
    while http_port == sql_port:
        http_port = port()
    admin, database_password, scrape, reader_password = (secrets.token_hex(32) for _ in range(4))
    base = f"http://127.0.0.1:{http_port}"
    otlp_requests = []
    collector_port = port()
    while collector_port in (sql_port, http_port):
        collector_port = port()

    class Collector(BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length).decode("utf-8", "replace")
            otlp_requests.append((self.path, body))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_args):
            pass

    collector = ThreadingHTTPServer(("127.0.0.1", collector_port), Collector)
    collector_thread = threading.Thread(target=collector.serve_forever, daemon=True)
    collector_thread.start()

    def request(path, token="", method="GET", body=None):
        headers = {"Content-Type": "application/json"}
        if token:
            headers["Authorization"] = "Bearer " + token
        req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=3) as response:
                return response.status, response.read(2 << 20).decode()
        except urllib.error.HTTPError as response:
            return response.code, ""

    with tempfile.TemporaryDirectory(prefix="mydb-native-monitoring-") as temporary:
        root = Path(temporary).resolve()
        if os.name == "nt":
            import getpass
            protected = subprocess.run(["icacls", str(root), "/inheritance:r", "/grant:r", getpass.getuser() + ":(OI)(CI)F", "*S-1-5-18:(OI)(CI)F"], capture_output=True)
            check(protected.returncode == 0, "Cannot protect the native fixture")
        else:
            root.chmod(0o700)
        credential = root / "metrics.token"
        credential.write_text(scrape, encoding="ascii")
        credential.chmod(0o600)
        config = {
            "server": {"host": "127.0.0.1", "port": sql_port, "max_connections": 32, "thread_count": 1},
            "http": {"host": "127.0.0.1", "port": http_port, "enabled": True, "admin_username": "ci-admin", "admin_password": admin},
            "storage": {"data_dir": str(root / "data"), "buffer_pool_size": "8M", "log_file_size": "16M", "group_commit_window_us": 0},
            "memory": {"max_memory": "128M"},
            "security": {"default_username": "root", "default_password": database_password, "enforce_strong_passwords": True, "local_infile": False},
            "backup": {"backup_dir": str(root / "backups"), "auto_backup_interval": 0},
            "logging": {"file": "", "level": "info"},
            "observability": {
                "metrics_token_file": str(credential),
                "otlp": {
                    "enabled": True,
                    "endpoint": f"http://127.0.0.1:{collector_port}",
                    "interval_secs": 1,
                    "timeout_secs": 2,
                    "service_name": "mydb-ci",
                    "export_traces": True,
                    "export_logs": True,
                    "include_sql": False,
                },
            },
        }
        config_file = root / "config.json"
        config_file.write_text(json.dumps(config), encoding="utf-8")
        config_file.chmod(0o600)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("MYDB_")}
        process = None
        with (root / "server.log").open("wb") as log:
            try:
                process = subprocess.Popen([str(server), "--config", str(config_file)], stdout=log, stderr=log, env=environment)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    check(process.poll() is None, "Native server exited before readiness")
                    try:
                        if request("/metrics", scrape)[0] == 200:
                            break
                    except (OSError, urllib.error.URLError):
                        pass
                    time.sleep(0.1)
                else:
                    raise RuntimeError("Native server did not become ready within 30 seconds")
                check(request("/metrics")[0] == 401, "Anonymous metrics access succeeded")
                check(request("/healthz")[0] == 200, "Liveness probe failed")
                check(request("/readyz")[0] == 200, "Readiness probe failed")
                check(request("/api/v1/config", scrape)[0] == 401, "Scrape credential reached management")
                check(request("/api/v1/sql/query", scrape, "POST", {"sql": "SELECT 1"})[0] == 401, "Scrape credential reached SQL")
                status, body = request("/api/v1/auth/login", method="POST", body={"username": "ci-admin", "password": admin})
                check(status == 200, "Administrator login failed")
                account = json.loads(body)
                check(account.get("administrator") is True, "Administrator role was not established")
                session = account["token"]

                def sql(statement):
                    check(request("/api/v1/sql/query", session, "POST", {"sql": statement})[0] == 200, "Native SQL fixture failed")

                sql("CREATE DATABASE ci_monitor")
                sql("CREATE TABLE ci_monitor.probe (id INT PRIMARY KEY, value VARCHAR(100))")
                sql("INSERT INTO ci_monitor.probe VALUES (1, 'private-native-smoke-value')")
                sql("CREATE TABLE ci_monitor.olap_probe (id INT, value VARCHAR(100))")
                values = ",".join(f"({index}, 'v{index:04d}')" for index in range(900))
                sql(f"INSERT INTO ci_monitor.olap_probe VALUES {values}")
                check(request("/api/v1/memory/flush", session, "POST", {})[0] == 200,
                      "Consistent flush failed")
                status, body = request(
                    "/api/v1/sql/query",
                    session,
                    "POST",
                    {"sql": "SELECT * FROM ci_monitor.olap_probe WHERE value BETWEEN 'v0100' AND 'v0199'"},
                )
                check(status == 200 and "v0100" in body and "v0199" in body and "v0200" not in body,
                      "OLAP zone-map range scan returned the wrong rows")
                status, body = request("/api/v1/observability/otlp", session)
                check(status == 200 and json.loads(body).get("auth_method") in {"none", "basic", "bearer"},
                      "OTLP status endpoint did not report a valid auth mode")
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline:
                    if any(path == "/v1/metrics" for path, _ in otlp_requests) and any(
                        path == "/v1/logs" for path, _ in otlp_requests
                    ):
                        break
                    time.sleep(0.2)
                check(any(path == "/v1/metrics" for path, _ in otlp_requests),
                      "OTLP metrics were not exported")
                check(any(path == "/v1/logs" for path, _ in otlp_requests),
                      "OTLP logs were not exported")
                log_payload = next(json.loads(body) for path, body in otlp_requests if path == "/v1/logs")
                check(bool(log_payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"]),
                      "OTLP logs payload had no records")
                metrics_payload = next(json.loads(body) for path, body in otlp_requests if path == "/v1/metrics")
                check(bool(metrics_payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]),
                      "OTLP metrics payload had no metrics")
                sql(f"CREATE USER 'ci_reader'@'%' IDENTIFIED BY '{reader_password}'")
                sql("GRANT SELECT ON ci_monitor.* TO 'ci_reader'@'%'")
                status, body = request("/api/v1/auth/login", method="POST", body={"username": "ci_reader", "password": reader_password})
                check(status == 200, "SQL reader login failed")
                account = json.loads(body)
                check(account.get("administrator") is False, "SQL reader received administrator role")
                reader = account["token"]
                check(request("/api/v1/sql/query", reader, "POST", {"sql": "SELECT * FROM ci_monitor.probe"})[0] == 200, "SQL reader lost its SELECT grant")
                for path in ("/api/v1/config", "/api/v1/status", "/api/v1/backup/list", "/metrics"):
                    check(request(path, reader)[0] == 403, "SQL reader reached a management resource")
                result = subprocess.run([str(client), "-h", "127.0.0.1", "-P", str(sql_port), "-u", "root", "-p", database_password, "-e", "SELECT VERSION()"], capture_output=True, text=True, timeout=10, env=environment)
                check(result.returncode == 0 and ("mydb-" + args.expected_version) in result.stdout, "Packaged native CLI/protocol version mismatch")
                metrics = request("/metrics", scrape)[1]
                pruned = next(
                    (int(line.split()[-1]) for line in metrics.splitlines()
                     if line.startswith("mydb_olap_pages_pruned_total ")),
                    0,
                )
                check(pruned > 0, "OLAP zone map did not prune any page")
                for private in (admin, database_password, scrape, reader_password, "private-native-smoke-value", "ci_reader"):
                    check(private not in metrics, "Public metrics disclosed private fixture data")
            finally:
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
        total = sum(path.stat().st_size for path in root.rglob("*") if path.is_file())
        check(total <= 64 << 20, "Native monitoring smoke exceeded its 64 MiB fixture budget")
    collector.shutdown()
    collector.server_close()
    print(json.dumps({"version": args.expected_version, "native_cli_protocol": True, "read_only_metrics": True, "sql_management_isolation": True, "sql_privacy": True, "otlp_metrics_logs": True, "temporary_bytes": total, "temporary_data_removed": True}))


if __name__ == "__main__":
    try:
        main()
    except RuntimeError as error:
        print("Native monitoring smoke failed: " + str(error), file=sys.stderr)
        sys.exit(1)
    except Exception as error:
        # Timeout/process exceptions can include password arguments. Never
        # serialize the exception or a traceback from this credential fixture.
        print("Native monitoring smoke failed: " + type(error).__name__, file=sys.stderr)
        sys.exit(1)
