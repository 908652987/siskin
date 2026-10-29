//! Provider endpoint presets and default ports.

use crate::{CoreError, ReceiveProtocol, SecurityMode};

#[derive(Debug, Clone)]
pub struct Hosts {
    pub provider: String,
    pub imap: String,
    pub pop3: String,
    pub smtp: String,
}

/// Known provider hosts. `88` covers the Perfect Mail domains 88.com, 111.com
/// and email.cn, each with its own host names.
pub fn hosts(provider: &str, email: &str) -> Result<Hosts, CoreError> {
    let provider = provider.to_ascii_lowercase();
    match provider.as_str() {
        "gmail" => Ok(Hosts {
            provider,
            imap: "imap.gmail.com".into(),
            pop3: "pop.gmail.com".into(),
            smtp: "smtp.gmail.com".into(),
        }),
        "qq" => Ok(Hosts {
            provider,
            imap: "imap.qq.com".into(),
            pop3: "pop.qq.com".into(),
            smtp: "smtp.qq.com".into(),
        }),
        "88" => {
            let domain = email
                .rsplit_once('@')
                .map(|(_, domain)| domain.to_ascii_lowercase())
                .unwrap_or_default();
            if !matches!(domain.as_str(), "88.com" | "111.com" | "email.cn") {
                return Err(CoreError::Settings(format!(
                    "88 mailbox domain must be 88.com, 111.com or email.cn, got {domain}"
                )));
            }
            Ok(Hosts {
                provider,
                imap: format!("imap.{domain}"),
                pop3: format!("pop.{domain}"),
                smtp: format!("smtp.{domain}"),
            })
        }
        "custom" => Err(CoreError::Settings(
            "custom provider needs explicit --host and --smtp-host".into(),
        )),
        other => Err(CoreError::Settings(format!("unknown provider: {other}"))),
    }
}

/// Standard receive port: implicit TLS uses the dedicated port, STARTTLS and
/// plaintext use the standard port.
pub fn default_port(protocol: ReceiveProtocol, security: SecurityMode) -> u16 {
    match (protocol, security) {
        (ReceiveProtocol::Imap, SecurityMode::Tls) => 993,
        (ReceiveProtocol::Imap, _) => 143,
        (ReceiveProtocol::Pop3, SecurityMode::Tls) => 995,
        (ReceiveProtocol::Pop3, _) => 110,
    }
}

pub fn default_smtp_port(security: SecurityMode) -> u16 {
    match security {
        SecurityMode::Tls => 465,
        SecurityMode::StartTls | SecurityMode::Plaintext => 587,
    }
}
