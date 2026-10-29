//! Mail protocol clients: IMAP transport, POP3 receive and SMTP submission.

use std::collections::HashSet;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::imap_proto::types::Address;
use async_imap::types::{Capability, Flag, Name};
use async_imap::Session;
use chrono::{DateTime, FixedOffset};
use futures::TryStreamExt;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub mod pop3;
pub mod smtp;

/// Transport security for protocol connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Implicit TLS from the first byte (993, 995, 465).
    Tls,
    /// Plaintext connection that is upgraded before credentials are sent.
    StartTls,
    /// No transport encryption; only for explicit local/testing use.
    Plaintext,
}

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("imap: {0}")]
    Imap(#[from] async_imap::error::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("server requires OAuth2 ({0}); password login is disabled")]
    OAuthRequired(String),
    #[error("unexpected server response: {0}")]
    Unexpected(String),
}

pub type Result<T, E = ProtoError> = std::result::Result<T, E>;

/// Sync strategy derived from what the server advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// QRESYNC + CONDSTORE: change-sequence based incremental sync.
    A,
    /// No CONDSTORE but IDLE: UID-range incremental plus periodic flag resync.
    B,
    /// No IDLE: polling only.
    C,
}

#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    pub idle: bool,
    pub condstore: bool,
    pub qresync: bool,
    pub can_move: bool,
    pub uidplus: bool,
    pub enabled: Vec<String>,
}

impl CapabilitySet {
    pub fn tier(&self) -> Tier {
        if self.qresync && self.condstore {
            Tier::A
        } else if self.idle {
            Tier::B
        } else {
            Tier::C
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub name: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SelectedMailbox {
    pub exists: u32,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct FetchedMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub size: Option<u32>,
    pub internal_date: Option<i64>,
    pub subject: Option<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub message_id: Option<String>,
    pub sent_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    NewData(String),
    Timeout,
    Interrupted,
}

#[derive(Debug)]
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl Stream {
    /// Returns the raw socket when no TLS has been applied yet, for STARTTLS.
    pub(crate) fn into_plain_tcp(self) -> Option<TcpStream> {
        match self {
            Stream::Plain(tcp) => Some(tcp),
            Stream::Tls(_) => None,
        }
    }
}

/// Wraps a TCP stream in TLS with the bundled webpki roots.
pub(crate) async fn tls_wrap(host: &str, tcp: TcpStream) -> Result<Stream> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls));
    let server_name =
        ServerName::try_from(host.to_string()).map_err(|e| ProtoError::Tls(e.to_string()))?;
    let secure = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ProtoError::Tls(e.to_string()))?;
    Ok(Stream::Tls(Box::new(secure)))
}

pub struct ImapSession {
    inner: Option<Session<Stream>>,
    capabilities: CapabilitySet,
}

impl ImapSession {
    pub async fn connect(config: &ImapConfig) -> Result<(Self, CapabilitySet)> {
        let tcp = TcpStream::connect((config.host.as_str(), config.port)).await?;
        tcp.set_nodelay(true).ok();

        let mut client = match config.security {
            Security::Tls => async_imap::Client::new(tls_wrap(&config.host, tcp).await?),
            Security::StartTls | Security::Plaintext => async_imap::Client::new(Stream::Plain(tcp)),
        };

        // Client::new leaves the greeting unread; consume it before any command.
        let greeting = client.read_response().await.map_err(ProtoError::Io)?;
        let Some(greeting) = greeting else {
            return Err(ProtoError::Unexpected(
                "server closed the connection before the greeting".into(),
            ));
        };
        if let async_imap::imap_proto::Response::Done { status, .. } = greeting.parsed() {
            if *status != async_imap::imap_proto::Status::Ok {
                return Err(ProtoError::Unexpected(format!(
                    "server rejected the connection: {status:?}"
                )));
            }
        }

        if config.security == Security::StartTls {
            client
                .run_command_and_check_ok("STARTTLS", None)
                .await
                .map_err(ProtoError::Imap)?;
            let plain = client.into_inner();
            let tcp = plain.into_plain_tcp().ok_or_else(|| {
                ProtoError::Unexpected("STARTTLS requested on an already encrypted stream".into())
            })?;
            client = async_imap::Client::new(tls_wrap(&config.host, tcp).await?);
        }

        let mut session = client
            .login(&config.username, &config.password)
            .await
            .map_err(|(err, _client)| ProtoError::Imap(err))?;

        // Re-check capabilities after STARTTLS; the server may advertise more.
        let raw = session.capabilities().await?;
        let capabilities = CapabilitySet {
            idle: has(&raw, "IDLE"),
            condstore: has(&raw, "CONDSTORE"),
            qresync: has(&raw, "QRESYNC"),
            can_move: has(&raw, "MOVE"),
            uidplus: has(&raw, "UIDPLUS"),
            enabled: raw.iter().map(capability_name).collect(),
        };

        Ok((
            Self {
                inner: Some(session),
                capabilities: capabilities.clone(),
            },
            capabilities,
        ))
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    pub fn tier(&self) -> Tier {
        self.capabilities.tier()
    }

    fn session(&mut self) -> &mut Session<Stream> {
        self.inner.as_mut().expect("session taken")
    }

    pub async fn list_mailboxes(&mut self) -> Result<Vec<MailboxInfo>> {
        let mut stream = self.session().list(Some(""), Some("*")).await?;
        let mut out = Vec::new();
        while let Some(item) = stream.try_next().await? {
            out.push(convert_name(&item));
        }
        Ok(out)
    }

    pub async fn select(&mut self, mailbox: &str) -> Result<SelectedMailbox> {
        let selected = if self.capabilities.condstore {
            self.session().select_condstore(mailbox).await?
        } else {
            self.session().select(mailbox).await?
        };
        Ok(SelectedMailbox {
            exists: selected.exists,
            uid_validity: selected.uid_validity,
            uid_next: selected.uid_next,
            highest_modseq: selected.highest_modseq,
        })
    }

    /// Fetches headers and flags for a UID range (`1:*`, `42:*`, ...).
    pub async fn fetch_range(&mut self, range: &str) -> Result<Vec<FetchedMessage>> {
        self.fetch(range, "(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
            .await
    }

    pub async fn fetch_flags(&mut self, range: &str) -> Result<Vec<FetchedMessage>> {
        self.fetch(range, "(UID FLAGS)").await
    }

    /// Flags-only fetch limited to messages with a newer mod-sequence (tier A).
    pub async fn fetch_flags_changed_since(&mut self, modseq: u64) -> Result<Vec<FetchedMessage>> {
        self.fetch("1:*", &format!("(UID FLAGS) (CHANGEDSINCE {modseq})"))
            .await
    }

    async fn fetch(&mut self, range: &str, query: &str) -> Result<Vec<FetchedMessage>> {
        let mut stream = self.session().uid_fetch(range, query).await?;
        let mut out = Vec::new();
        while let Some(item) = stream.try_next().await? {
            out.push(convert_fetch(&item));
        }
        drop(stream);
        Ok(out)
    }

    pub async fn list_uids(&mut self) -> Result<HashSet<u32>> {
        let set = self.session().uid_search("ALL").await?;
        Ok(set.into_iter().collect())
    }

    pub async fn append(&mut self, mailbox: &str, flags: Option<&str>, body: &[u8]) -> Result<()> {
        self.session().append(mailbox, flags, None, body).await?;
        Ok(())
    }

    pub async fn add_flag(&mut self, uid: u32, flag: &str) -> Result<()> {
        let mut stream = self
            .session()
            .uid_store(uid.to_string(), format!("+FLAGS.SILENT ({flag})"))
            .await?;
        while stream.try_next().await?.is_some() {}
        Ok(())
    }

    /// Moves a message, using MOVE when available and copy+delete otherwise.
    pub async fn move_message(&mut self, uid: u32, destination: &str) -> Result<&'static str> {
        if self.capabilities.can_move {
            self.session().uid_mv(uid.to_string(), destination).await?;
            Ok("move")
        } else {
            self.session()
                .uid_copy(uid.to_string(), destination)
                .await?;
            let mut stream = self
                .session()
                .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                .await?;
            while stream.try_next().await?.is_some() {}
            drop(stream);
            let mut expunged = Box::pin(self.session().uid_expunge(uid.to_string()).await?);
            while expunged.try_next().await?.is_some() {}
            Ok("copy+delete")
        }
    }

    /// Sends IDLE and waits for the first mailbox change or timeout.
    pub async fn idle_wait(&mut self, timeout: Duration) -> Result<IdleEvent> {
        let mut handle = self.inner.take().expect("session taken").idle();
        handle.init().await?;
        let (wait, _stop) = handle.wait_with_timeout(timeout);
        let outcome = wait.await;
        let session = handle.done().await?;
        self.inner = Some(session);
        match outcome {
            Ok(async_imap::extensions::idle::IdleResponse::NewData(data)) => {
                Ok(IdleEvent::NewData(format!("{:?}", data.parsed())))
            }
            Ok(async_imap::extensions::idle::IdleResponse::Timeout) => Ok(IdleEvent::Timeout),
            Ok(async_imap::extensions::idle::IdleResponse::ManualInterrupt) => {
                Ok(IdleEvent::Interrupted)
            }
            Err(err) => Err(ProtoError::Imap(err)),
        }
    }

    pub async fn logout(mut self) {
        if let Some(mut session) = self.inner.take() {
            session.logout().await.ok();
        }
    }
}

/// Reads one CRLF-terminated control line, rejecting oversized or unterminated input.
pub(crate) async fn read_line(io: &mut BufReader<Stream>) -> Result<String> {
    let mut line = String::new();
    let read = io.read_line(&mut line).await?;
    if read == 0 {
        return Err(ProtoError::Unexpected("connection closed".into()));
    }
    if line.len() > 1024 * 1024 {
        return Err(ProtoError::Unexpected("protocol line too long".into()));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Writes one CRLF-terminated line and flushes it.
pub(crate) async fn write_line(io: &mut BufReader<Stream>, line: &str) -> Result<()> {
    let stream = io.get_mut();
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await?;
    Ok(())
}

fn has(caps: &async_imap::types::Capabilities, name: &str) -> bool {
    caps.has_str(name)
}

fn capability_name(cap: &Capability) -> String {
    match cap {
        Capability::Imap4rev1 => "IMAP4rev1".to_string(),
        Capability::Auth(v) => format!("AUTH={v}"),
        Capability::Atom(v) => v.clone(),
    }
}

fn convert_name(name: &Name) -> MailboxInfo {
    MailboxInfo {
        name: name.name().to_string(),
        attributes: name.attributes().iter().map(|a| format!("{a:?}")).collect(),
    }
}

fn convert_fetch(item: &async_imap::types::Fetch) -> FetchedMessage {
    let mut out = FetchedMessage {
        uid: item.uid.unwrap_or_default(),
        flags: item.flags().map(flag_name).collect(),
        size: item.size,
        internal_date: item.internal_date().map(|dt| dt.timestamp()),
        ..Default::default()
    };
    if let Some(env) = item.envelope() {
        out.subject = env
            .subject
            .as_ref()
            .map(|v| String::from_utf8_lossy(v).to_string());
        out.message_id = env
            .message_id
            .as_ref()
            .map(|v| String::from_utf8_lossy(v).to_string());
        out.from = env
            .from
            .as_ref()
            .map(|v| v.iter().map(format_address).collect())
            .unwrap_or_default();
        out.to = env
            .to
            .as_ref()
            .map(|v| v.iter().map(format_address).collect())
            .unwrap_or_default();
        out.sent_at = env
            .date
            .as_ref()
            .and_then(|raw| {
                DateTime::<FixedOffset>::parse_from_rfc2822(&String::from_utf8_lossy(raw)).ok()
            })
            .map(|dt| dt.timestamp());
    }
    out
}

fn flag_name(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(v) => v.to_string(),
    }
}

fn format_address(addr: &Address<'_>) -> String {
    let mailbox = addr
        .mailbox
        .as_ref()
        .map(|v| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    let host = addr
        .host
        .as_ref()
        .map(|v| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    let email = if host.is_empty() {
        mailbox
    } else {
        format!("{mailbox}@{host}")
    };
    match addr
        .name
        .as_ref()
        .map(|v| String::from_utf8_lossy(v).to_string())
    {
        Some(name) if !name.is_empty() => format!("{name} <{email}>"),
        _ => email,
    }
}
