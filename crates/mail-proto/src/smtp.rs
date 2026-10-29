//! Minimal SMTP submission client: EHLO, STARTTLS, AUTH, DATA.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::{read_line, tls_wrap, write_line, ProtoError, Result, Security, Stream};

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub struct SmtpSession {
    io: BufReader<Stream>,
    capabilities: Vec<String>,
}

impl SmtpSession {
    pub async fn connect(config: &SmtpConfig) -> Result<Self> {
        let tcp = TcpStream::connect((config.host.as_str(), config.port)).await?;
        tcp.set_nodelay(true).ok();
        let stream = match config.security {
            Security::Tls => tls_wrap(&config.host, tcp).await?,
            Security::StartTls | Security::Plaintext => Stream::Plain(tcp),
        };
        let mut session = Self {
            io: BufReader::new(stream),
            capabilities: Vec::new(),
        };

        let greeting = read_line(&mut session.io).await?;
        if !greeting.starts_with("220") {
            return Err(ProtoError::Unexpected("SMTP greeting rejected".into()));
        }
        session.capabilities = session.ehlo().await?;

        if config.security == Security::StartTls {
            if !session
                .capabilities
                .iter()
                .any(|cap| cap.eq_ignore_ascii_case("STARTTLS"))
            {
                return Err(ProtoError::Unexpected(
                    "server does not advertise STARTTLS".into(),
                ));
            }
            let reply = session.command("STARTTLS").await?;
            if !reply.starts_with("220") {
                return Err(ProtoError::Unexpected("STARTTLS was refused".into()));
            }
            if !session.io.buffer().is_empty() {
                return Err(ProtoError::Unexpected(
                    "unexpected data buffered before the TLS handshake".into(),
                ));
            }
            let stream = session.io.into_inner();
            let tcp = stream
                .into_plain_tcp()
                .ok_or_else(|| ProtoError::Unexpected("STARTTLS on an encrypted stream".into()))?;
            session = Self {
                io: BufReader::new(tls_wrap(&config.host, tcp).await?),
                capabilities: Vec::new(),
            };
            session.capabilities = session.ehlo().await?;
        }

        if let Some(username) = config.username.as_deref() {
            let password = config.password.as_deref().unwrap_or_default();
            session.authenticate(username, password).await?;
        }
        Ok(session)
    }

    /// Submit one message. `from` and the recipients are envelope addresses.
    pub async fn send(&mut self, from: &str, recipients: &[String], message: &[u8]) -> Result<()> {
        if recipients.is_empty() {
            return Err(ProtoError::Unexpected("no recipients".into()));
        }
        let reply = self.command(&format!("MAIL FROM:<{from}>")).await?;
        if !reply.starts_with("250") {
            return Err(ProtoError::Unexpected(format!(
                "MAIL FROM rejected ({})",
                status_code(&reply)
            )));
        }
        for recipient in recipients {
            let reply = self.command(&format!("RCPT TO:<{recipient}>")).await?;
            if !reply.starts_with("250") && !reply.starts_with("251") {
                return Err(ProtoError::Unexpected(format!(
                    "recipient {recipient} rejected ({})",
                    status_code(&reply)
                )));
            }
        }
        let reply = self.command("DATA").await?;
        if !reply.starts_with("354") {
            return Err(ProtoError::Unexpected("DATA was refused".into()));
        }
        self.write_data(message).await?;
        let reply = read_line(&mut self.io).await?;
        if !reply.starts_with("250") {
            return Err(ProtoError::Unexpected(format!(
                "message rejected after DATA ({})",
                status_code(&reply)
            )));
        }
        Ok(())
    }

    pub async fn quit(mut self) {
        let _ = self.command("QUIT").await;
    }

    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        let plain = self.capabilities.iter().any(|line| {
            line.to_ascii_uppercase()
                .split_whitespace()
                .any(|t| t == "PLAIN")
        });
        let login = self.capabilities.iter().any(|line| {
            line.to_ascii_uppercase()
                .split_whitespace()
                .any(|t| t == "LOGIN")
        });
        if plain {
            let payload = STANDARD.encode(format!("\0{username}\0{password}"));
            let reply = self.command(&format!("AUTH PLAIN {payload}")).await?;
            if !reply.starts_with("235") {
                return Err(ProtoError::Unexpected("SMTP authentication failed".into()));
            }
            return Ok(());
        }
        if login {
            let reply = self.command("AUTH LOGIN").await?;
            if !reply.starts_with("334") {
                return Err(ProtoError::Unexpected("AUTH LOGIN was refused".into()));
            }
            let reply = self.command(&STANDARD.encode(username)).await?;
            if !reply.starts_with("334") {
                return Err(ProtoError::Unexpected("AUTH LOGIN username refused".into()));
            }
            let reply = self.command(&STANDARD.encode(password)).await?;
            if !reply.starts_with("235") {
                return Err(ProtoError::Unexpected("SMTP authentication failed".into()));
            }
            return Ok(());
        }
        Err(ProtoError::Unexpected(
            "server advertises no supported AUTH mechanism".into(),
        ))
    }

    async fn ehlo(&mut self) -> Result<Vec<String>> {
        write_line(&mut self.io, "EHLO localhost").await?;
        let first = read_line(&mut self.io).await?;
        if !first.starts_with("250") {
            return Err(ProtoError::Unexpected(format!(
                "EHLO rejected ({})",
                status_code(&first)
            )));
        }
        let mut capabilities = Vec::new();
        let mut line = first;
        loop {
            if line.len() > 4 {
                capabilities.push(line[4..].trim().to_string());
            }
            if line.as_bytes().get(3) != Some(&b'-') {
                break;
            }
            line = read_line(&mut self.io).await?;
            if !line.starts_with("250") {
                return Err(ProtoError::Unexpected("malformed EHLO response".into()));
            }
        }
        Ok(capabilities)
    }

    async fn write_data(&mut self, message: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(message);
        for raw in text.split_terminator('\n') {
            let line = raw.trim_end_matches('\r');
            let line = if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_string()
            };
            let stream = self.io.get_mut();
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\r\n").await?;
        }
        let stream = self.io.get_mut();
        stream.write_all(b".\r\n").await?;
        stream.flush().await?;
        Ok(())
    }

    async fn command(&mut self, command: &str) -> Result<String> {
        write_line(&mut self.io, command).await?;
        read_line(&mut self.io).await
    }
}

fn status_code(reply: &str) -> String {
    reply.split_whitespace().next().unwrap_or("?").to_string()
}
