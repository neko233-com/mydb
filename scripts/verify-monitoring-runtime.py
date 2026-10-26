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
import time
import urllib.error
import urllib.request


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
            "logging": {"file": "", "level": "warn"},
            "observability": {"metrics_token_file": str(credential)},
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
    print(json.dumps({"version": args.expected_version, "native_cli_protocol": True, "read_only_metrics": True, "sql_management_isolation": True, "sql_privacy": True, "temporary_bytes": total, "temporary_data_removed": True}))


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
