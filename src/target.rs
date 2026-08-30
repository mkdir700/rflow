use std::{fmt, net::SocketAddr, str::FromStr};

use anyhow::{Context, Result, bail};

pub const DEFAULT_PORT: u16 = 24_801;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTarget {
    host: String,
    port: u16,
}

impl ServerTarget {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn resolve(&self) -> Result<SocketAddr> {
        tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("resolve server target {self}"))?
            .next()
            .with_context(|| format!("server target {self} resolved to no addresses"))
    }
}

impl fmt::Display for ServerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(formatter, "[{}]:{}", self.host, self.port)
        } else {
            write!(formatter, "{}:{}", self.host, self.port)
        }
    }
}

impl FromStr for ServerTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() || value.chars().any(char::is_whitespace) {
            bail!("server target must be an IP address or hostname");
        }

        if let Ok(address) = value.parse::<SocketAddr>() {
            return Ok(Self {
                host: address.ip().to_string(),
                port: address.port(),
            });
        }

        if value.starts_with('[') || value.ends_with(']') {
            bail!("invalid bracketed server target {value}");
        }

        if value.matches(':').count() == 1 {
            let (host, port) = value.rsplit_once(':').expect("one colon is present");
            if host.is_empty() {
                bail!("server target hostname cannot be empty");
            }
            let port = port
                .parse::<u16>()
                .with_context(|| format!("invalid server target port {port}"))?;
            return Ok(Self {
                host: host.to_owned(),
                port,
            });
        }

        Ok(Self {
            host: value.to_owned(),
            port: DEFAULT_PORT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_explicit_hostname_port() {
        let target: ServerTarget = "desktop.local:25000".parse().unwrap();
        assert_eq!(target.host(), "desktop.local");
        assert_eq!(target.port(), 25_000);
        assert_eq!(target.to_string(), "desktop.local:25000");
    }

    #[test]
    fn formats_bare_ipv6_with_default_port() {
        let target: ServerTarget = "::1".parse().unwrap();
        assert_eq!(target.to_string(), "[::1]:24801");
    }
}
