//! POP3 client for receive-only accounts.
//!
//! POP3 has no folders, flags, or IDLE: the sync layer mirrors the single
//! mailbox by UIDL and polls.

use tokio::io::BufReader;
use tokio::net::TcpStream;

use crate::{read_line, tls_wrap, write_line, ProtoError, Result, Security, Stream};

pub struct Pop3Config {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Pop3Message {
    /// Sequence number valid only for the current session.
    pub number: u32,
    /// Stable unique identifier persisted by the server.
    pub uid: String,
    pub size: u64,
}

pub struct Pop3Session {
    io: BufReader<Stream>,
}

impl Pop3Session {
    pub async fn connect(config: &Pop3Config) -> Result<Self> {
        let tcp = TcpStream::connect((config.host.as_str(), config.port)).await?;
        tcp.set_nodelay(true).ok();
        let stream = match config.security {
            Security::Tls => tls_wrap(&config.host, tcp).await?,
            Security::StartTls | Security::Plaintext => Stream::Plain(tcp),
        };
        let mut session = Self {
            io: BufReader::new(stream),
        };
        let greeting = read_line(&mut session.io).await?;
        if !greeting.starts_with("+OK") {
            return Err(ProtoError::Unexpected("POP3 greeting rejected".into()));
        }

        let capabilities = session.capabilities().await?;
        if config.security == Security::StartTls {
            if !capabilities
                .iter()
                .any(|cap| cap.eq_ignore_ascii_case("STLS"))
            {
                return Err(ProtoError::Unexpected(
                    "server does not advertise STLS".into(),
                ));
            }
            let reply = session.command("STLS").await?;
            if !reply.starts_with("+OK") {
                return Err(ProtoError::Unexpected("STLS was refused".into()));
            }
            // Anything buffered here would be plaintext that belongs after the
            // TLS handshake, so refuse rather than silently dropping it.
            if !session.io.buffer().is_empty() {
                return Err(ProtoError::Unexpected(
                    "unexpected data buffered before the TLS handshake".into(),
                ));
            }
            let stream = session.io.into_inner();
            let tcp = stream
                .into_plain_tcp()
                .ok_or_else(|| ProtoError::Unexpected("STLS on an encrypted stream".into()))?;
            session = Self {
                io: BufReader::new(tls_wrap(&config.host, tcp).await?),
            };
        }

        let reply = session
            .command(&format!("USER {}", config.username))
            .await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected("POP3 USER was rejected".into()));
        }
        let reply = session
            .command(&format!("PASS {}", config.password))
            .await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected("POP3 authentication failed".into()));
        }
        Ok(session)
    }

    pub async fn list(&mut self) -> Result<Vec<Pop3Message>> {
        let sizes = self.list_sizes().await?;
        let uids = self.uids().await?;
        let mut out = Vec::with_capacity(uids.len());
        for (number, uid) in uids {
            let size = sizes
                .iter()
                .find(|(n, _)| *n == number)
                .map(|(_, size)| *size)
                .unwrap_or(0);
            out.push(Pop3Message { number, uid, size });
        }
        Ok(out)
    }

    /// Header block only; falls back to RETR when TOP is unsupported.
    pub async fn headers(&mut self, number: u32) -> Result<String> {
        let reply = self.command(&format!("TOP {number} 0")).await?;
        if reply.starts_with("+OK") {
            let lines = self.multiline().await?;
            return Ok(lines.join("\r\n"));
        }
        let reply = self.command(&format!("RETR {number}")).await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected(format!(
                "POP3 RETR {number} was refused"
            )));
        }
        let lines = self.multiline().await?;
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            headers.push(line);
        }
        Ok(headers.join("\r\n"))
    }

    pub async fn quit(mut self) {
        let _ = self.command("QUIT").await;
    }

    /// Retrieves the whole message. POP3 has no partial fetch, so bodies and
    /// attachments arrive together and the caller parses them out.
    ///
    /// Lines are rejoined with CRLF and dot-stuffed lines are unstuffed, so the
    /// result is semantically identical but not byte-identical to the original.
    pub async fn retrieve(&mut self, number: u32) -> Result<Vec<u8>> {
        let reply = self.command(&format!("RETR {number}")).await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected(format!(
                "POP3 RETR {number} was refused"
            )));
        }
        let lines = self.multiline().await?;
        let mut out = Vec::with_capacity(lines.iter().map(|line| line.len() + 2).sum());
        for line in lines {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        Ok(out)
    }

    async fn list_sizes(&mut self) -> Result<Vec<(u32, u64)>> {
        let reply = self.command("LIST").await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected("POP3 LIST was refused".into()));
        }
        let mut out = Vec::new();
        for line in self.multiline().await? {
            let mut parts = line.split_whitespace();
            let Some(number) = parts.next().and_then(|v| v.parse().ok()) else {
                continue;
            };
            let size = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            out.push((number, size));
        }
        Ok(out)
    }

    async fn uids(&mut self) -> Result<Vec<(u32, String)>> {
        let reply = self.command("UIDL").await?;
        if !reply.starts_with("+OK") {
            return Err(ProtoError::Unexpected(
                "server does not support UIDL".into(),
            ));
        }
        let mut out = Vec::new();
        for line in self.multiline().await? {
            let mut parts = line.splitn(2, ' ');
            let Some(number) = parts.next().and_then(|v| v.parse().ok()) else {
                continue;
            };
            let Some(uid) = parts.next() else {
                continue;
            };
            if !uid.is_empty() {
                out.push((number, uid.to_string()));
            }
        }
        Ok(out)
    }

    async fn capabilities(&mut self) -> Result<Vec<String>> {
        let reply = self.command("CAPA").await?;
        if !reply.starts_with("+OK") {
            return Ok(Vec::new());
        }
        self.multiline().await
    }

    async fn multiline(&mut self) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            let line = read_line(&mut self.io).await?;
            if line == "." {
                break;
            }
            // Dot-stuffing prepends one dot, so remove exactly one.
            match line.strip_prefix('.') {
                Some(rest) => lines.push(rest.to_string()),
                None => lines.push(line),
            }
        }
        Ok(lines)
    }

    async fn command(&mut self, command: &str) -> Result<String> {
        write_line(&mut self.io, command).await?;
        read_line(&mut self.io).await
    }
}
