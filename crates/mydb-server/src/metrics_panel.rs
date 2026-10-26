//! MetricsPanel233 uses authenticated Prometheus pull or the existing OTLP exporter.
//! Scrape credentials are deliberately isolated from SQL and management sessions.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use mydb_config::ServerConfig;

const MAX_TOKEN_BYTES: usize = 8192;

pub fn load_token(path: Option<&Path>) -> Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let file = std::fs::File::open(path).context("cannot open metrics credential file")?;
    read_token(file).map(Some)
}

fn read_token(reader: impl Read) -> Result<String> {
    // Read one byte beyond the largest valid token plus CRLF, so a large or
    // non-terminating input cannot consume unbounded startup memory.
    let mut value = String::new();
    reader
        .take((MAX_TOKEN_BYTES + 3) as u64)
        .read_to_string(&mut value)
        .context("cannot read metrics credential file")?;
    anyhow::ensure!(
        value.len() <= MAX_TOKEN_BYTES + 2,
        "metrics credential file exceeds the size limit"
    );
    let token = value.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(
        (32..=MAX_TOKEN_BYTES).contains(&token.len())
            && token.bytes().all(|b| b.is_ascii_graphic()),
        "metrics credential must contain 32..8192 printable non-whitespace ASCII bytes"
    );
    Ok(token.to_owned())
}

pub fn security_metrics(config: &ServerConfig, authentication_failures: u64) -> String {
    let mut body = String::new();
    for (name, value) in [
        (
            "mydb_max_connections",
            u64::from(config.server.max_connections),
        ),
        (
            "mydb_security_tls_configured",
            u64::from(config.security.tls_cert.is_some() && config.security.tls_key.is_some()),
        ),
        (
            "mydb_security_require_secure_transport",
            u64::from(config.security.require_secure_transport),
        ),
        (
            "mydb_security_strong_passwords_enforced",
            u64::from(config.security.enforce_strong_passwords),
        ),
        (
            "mydb_security_local_infile_enabled",
            u64::from(config.security.local_infile),
        ),
    ] {
        body.push_str(&format!("# TYPE {name} gauge\n{name} {value}\n"));
    }
    body.push_str(&format!("# TYPE mydb_authentication_failures_total counter\nmydb_authentication_failures_total {authentication_failures}\n"));
    body.push_str(&format!(
        "# TYPE mydb_build_info gauge\nmydb_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_bounded_and_errors_do_not_disclose_them() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("metrics.token");
        std::fs::write(&path, "short-private-value")?;
        let error = load_token(Some(&path)).expect_err("short credentials must fail");
        assert!(!error.to_string().contains("short-private-value"));
        let token = "x".repeat(64);
        std::fs::write(&path, format!("{token}\r\n"))?;
        assert_eq!(load_token(Some(&path))?, Some(token));
        assert_eq!(load_token(None)?, None);
        Ok(())
    }

    #[test]
    fn security_metrics_report_actual_configuration_without_credentials() {
        let mut config = ServerConfig::default();
        config.security.default_password = "private-database-password".into();
        config.security.require_secure_transport = true;
        config.security.enforce_strong_passwords = true;
        config.security.local_infile = false;
        let body = security_metrics(&config, 3);
        assert!(body.contains("mydb_authentication_failures_total 3\n"));
        assert!(body.contains("mydb_security_require_secure_transport 1\n"));
        assert!(body.contains("mydb_security_local_infile_enabled 0\n"));
        assert!(!body.contains("private-database-password"));
    }

    #[test]
    fn credential_reader_bounds_non_terminating_and_oversized_inputs() -> Result<()> {
        assert!(read_token(std::io::repeat(b'x')).is_err());
        let largest = "x".repeat(MAX_TOKEN_BYTES);
        assert_eq!(read_token(format!("{largest}\r\n").as_bytes())?, largest);
        assert!(read_token("x".repeat(MAX_TOKEN_BYTES + 1).as_bytes()).is_err());
        assert!(read_token(b"not-a-valid-credential-\xff-but-private".as_slice()).is_err());
        Ok(())
    }
}
