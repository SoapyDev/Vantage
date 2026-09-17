//! Optional email delivery of vantage reports.
//!
//! This crate isolates the mail dependency (`lettre`) so the rest of the app
//! stays lean: nothing here is compiled unless a mail feature is enabled. The
//! generic [`smtp`](self) transport is the workhorse; provider features
//! (`o365`, `gmail`, `protonmail`) add cheap host/port/security presets and
//! mark where heavier, provider-specific integrations would live.
//!
//! Configuration comes from the environment (see [`MailConfig::from_env`]), so
//! secrets stay in `.env` and never in the binary.

/// TLS posture for the SMTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Upgrade a plaintext connection with STARTTLS (submission, port 587).
    StartTls,
    /// Implicit TLS from the first byte (SMTPS, port 465).
    Tls,
    /// No TLS — only for a trusted local relay/catcher (e.g. Mailpit).
    Plain,
}

/// Where a message is delivered.
#[derive(Debug, Clone)]
pub enum Transport {
    /// A real SMTP relay.
    Smtp,
    /// Write the message as an `.eml` file into a directory. Handy for tests
    /// and local development — no network, nothing to deliver.
    File(std::path::PathBuf),
}

/// Everything needed to reach a mail relay. Built once, lazily, from the
/// environment; see [`MailConfig::from_env`].
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub from: String,
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: Option<String>,
    pub password: Option<String>,
    pub transport: Transport,
}

/// A report ready to be sent: recipients, subject, plain-text body, and the
/// single HTML attachment (the self-contained report).
#[derive(Debug, Clone)]
pub struct OutgoingReport {
    pub recipients: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachment_path: std::path::PathBuf,
    pub attachment_name: String,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl MailConfig {
    /// Reads the mail configuration from the environment.
    ///
    /// `SMTP_PROVIDER` selects a preset (`generic`, or a compiled-in provider
    /// such as `o365`/`gmail`/`protonmail`); `SMTP_HOST`/`SMTP_PORT`/
    /// `SMTP_SECURITY` override it. `SMTP_FROM` is always required.
    /// `SMTP_USERNAME`/`SMTP_PASSWORD` are required for the SMTP transport.
    /// `SMTP_TRANSPORT=file` (with `SMTP_FILE_DIR`, default `./mail-outbox`)
    /// swaps in the offline file transport.
    ///
    /// # Errors
    /// Returns an error if a required variable is missing or a value (port,
    /// security, provider) is invalid or not compiled in.
    pub fn from_env() -> anyhow::Result<Self> {
        let provider = env("SMTP_PROVIDER").unwrap_or_else(|| "generic".to_string());
        let (preset_host, preset_port, preset_security) = provider_preset(&provider)?;

        let host = env("SMTP_HOST")
            .or_else(|| preset_host.map(str::to_string))
            .ok_or_else(|| {
                anyhow::anyhow!("SMTP_HOST is required (or set SMTP_PROVIDER to a known provider)")
            })?;
        let from = env("SMTP_FROM").ok_or_else(|| anyhow::anyhow!("SMTP_FROM is required"))?;
        let transport = transport_from_env()?;

        let username = env("SMTP_USERNAME");
        let password = env("SMTP_PASSWORD");
        if matches!(transport, Transport::Smtp) && (username.is_none() || password.is_none()) {
            anyhow::bail!("SMTP_USERNAME and SMTP_PASSWORD are required for the SMTP transport");
        }

        Ok(Self {
            from,
            host,
            port: port_from_env(preset_port)?,
            security: security_from_env(preset_security)?,
            username,
            password,
            transport,
        })
    }
}

/// `SMTP_PORT`, or the provider preset when absent.
fn port_from_env(preset: u16) -> anyhow::Result<u16> {
    match env("SMTP_PORT") {
        Some(p) => p
            .parse()
            .map_err(|_| anyhow::anyhow!("SMTP_PORT must be a number, got {p:?}")),
        None => Ok(preset),
    }
}

/// `SMTP_SECURITY` (`starttls`|`tls`|`plain`), or the provider preset when
/// absent.
fn security_from_env(preset: Security) -> anyhow::Result<Security> {
    match env("SMTP_SECURITY").as_deref() {
        None => Ok(preset),
        Some("starttls") => Ok(Security::StartTls),
        Some("tls") => Ok(Security::Tls),
        Some("plain") => Ok(Security::Plain),
        Some(other) => {
            anyhow::bail!("SMTP_SECURITY must be starttls|tls|plain, got {other:?}")
        }
    }
}

/// `SMTP_TRANSPORT`: the SMTP relay (default), or `file` (with
/// `SMTP_FILE_DIR`, default `./mail-outbox`) for the offline transport.
fn transport_from_env() -> anyhow::Result<Transport> {
    match env("SMTP_TRANSPORT").as_deref() {
        Some("file") => Ok(Transport::File(
            env("SMTP_FILE_DIR")
                .unwrap_or_else(|| "./mail-outbox".to_string())
                .into(),
        )),
        None | Some("smtp") => Ok(Transport::Smtp),
        Some(other) => anyhow::bail!("SMTP_TRANSPORT must be smtp|file, got {other:?}"),
    }
}

/// Resolves a provider preset to `(host, default_port, default_security)`.
/// `generic` has no host preset (the caller must supply `SMTP_HOST`). Unknown
/// or not-compiled providers are a hard error naming the feature to enable.
fn provider_preset(provider: &str) -> anyhow::Result<(Option<&'static str>, u16, Security)> {
    match provider {
        "generic" => Ok((None, 587, Security::StartTls)),
        #[cfg(feature = "o365")]
        "o365" => Ok((Some("smtp.office365.com"), 587, Security::StartTls)),
        #[cfg(feature = "gmail")]
        "gmail" => Ok((Some("smtp.gmail.com"), 587, Security::StartTls)),
        #[cfg(feature = "protonmail")]
        "protonmail" => Ok((Some("127.0.0.1"), 1025, Security::StartTls)),
        other => anyhow::bail!(
            "unknown or not-compiled SMTP provider {other:?}; rebuild with the matching feature \
             (e.g. --features email-{other})"
        ),
    }
}

#[cfg(feature = "smtp")]
mod send_impl {
    use super::{MailConfig, OutgoingReport, Security, Transport};
    use lettre::message::header::ContentType;
    use lettre::message::{Attachment, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
    use std::sync::OnceLock;

    /// Builds the MIME message: a plain-text body plus the HTML report as an
    /// attachment. Kept separate so it can be unit-tested without a transport.
    ///
    /// # Errors
    /// Returns an error if an address fails to parse or the attachment cannot
    /// be read.
    pub fn build_message(cfg: &MailConfig, report: &OutgoingReport) -> anyhow::Result<Message> {
        let mut builder = Message::builder()
            .from(
                cfg.from
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid SMTP_FROM {:?}: {e}", cfg.from))?,
            )
            .subject(report.subject.clone());
        for to in &report.recipients {
            builder = builder.to(to
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid recipient {to:?}: {e}"))?);
        }

        let bytes = std::fs::read(&report.attachment_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read report attachment {}: {e}",
                report.attachment_path.display()
            )
        })?;
        let attachment = Attachment::new(report.attachment_name.clone())
            .body(bytes, ContentType::parse("text/html; charset=utf-8")?);

        let message = builder.multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(report.body.clone()))
                .singlepart(attachment),
        )?;
        Ok(message)
    }

    /// Sends the report over the configured transport. The SMTP connection is
    /// built once and reused (see `smtp_transport`).
    ///
    /// # Errors
    /// Returns an error if the message cannot be built or the transport fails.
    pub async fn send(cfg: &MailConfig, report: OutgoingReport) -> anyhow::Result<()> {
        let message = build_message(cfg, &report)?;
        match &cfg.transport {
            Transport::File(dir) => {
                std::fs::create_dir_all(dir)?;
                let transport = AsyncFileTransport::<Tokio1Executor>::new(dir);
                transport
                    .send(message)
                    .await
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("writing the .eml failed: {e}"))
            }
            Transport::Smtp => smtp_transport(cfg)?
                .send(message)
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("SMTP send failed: {e}")),
        }
    }

    /// The SMTP transport, built on first use and cached for the process
    /// lifetime (connection pooling lives inside it).
    fn smtp_transport(
        cfg: &MailConfig,
    ) -> anyhow::Result<&'static AsyncSmtpTransport<Tokio1Executor>> {
        static TRANSPORT: OnceLock<AsyncSmtpTransport<Tokio1Executor>> = OnceLock::new();
        if let Some(existing) = TRANSPORT.get() {
            return Ok(existing);
        }
        let built = build_smtp_transport(cfg)?;
        // A racing initializer would only build an identical transport; keep
        // whichever landed first.
        let _ = TRANSPORT.set(built);
        Ok(TRANSPORT.get().expect("transport was just set"))
    }

    fn build_smtp_transport(
        cfg: &MailConfig,
    ) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
        let base = match cfg.security {
            Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?,
            Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)?,
            Security::Plain => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host),
        };
        let mut base = base.port(cfg.port);
        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            base = base.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        Ok(base.build())
    }
}

#[cfg(feature = "smtp")]
pub use send_impl::{build_message, send};

#[cfg(all(test, feature = "smtp"))]
mod tests {
    use super::*;

    fn write_html(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("index.html");
        std::fs::write(&p, "<html>report</html>").unwrap();
        p
    }

    fn cfg() -> MailConfig {
        MailConfig {
            from: "vantage <no-reply@example.com>".to_string(),
            host: "localhost".to_string(),
            port: 25,
            security: Security::Plain,
            username: None,
            password: None,
            transport: Transport::Smtp,
        }
    }

    #[test]
    fn build_message_sets_envelope_and_attaches_html() {
        let dir = std::env::temp_dir().join(format!("mailer_msg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = OutgoingReport {
            recipients: vec!["qa@example.com".to_string()],
            subject: "Live services".to_string(),
            body: "3 passed, 0 failed".to_string(),
            attachment_path: write_html(&dir),
            attachment_name: "index.html".to_string(),
        };

        let msg = build_message(&cfg(), &report).unwrap();
        let envelope = msg.envelope();
        assert_eq!(envelope.to().len(), 1);
        assert_eq!(envelope.to()[0].to_string(), "qa@example.com");

        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("Subject: Live services"), "{raw}");
        assert!(
            raw.contains("index.html"),
            "attachment filename present: {raw}"
        );
        assert!(raw.contains("text/html"), "attachment is html: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_message_rejects_a_bad_recipient() {
        let dir = std::env::temp_dir().join(format!("mailer_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = OutgoingReport {
            recipients: vec!["not-an-email".to_string()],
            subject: "x".to_string(),
            body: "y".to_string(),
            attachment_path: write_html(&dir),
            attachment_name: "index.html".to_string(),
        };
        assert!(build_message(&cfg(), &report).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_transport_writes_an_eml() {
        let dir = std::env::temp_dir().join(format!("mailer_file_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outbox = dir.join("outbox");
        let mut config = cfg();
        config.transport = Transport::File(outbox.clone());

        let report = OutgoingReport {
            recipients: vec!["qa@example.com".to_string()],
            subject: "Report".to_string(),
            body: "summary".to_string(),
            attachment_path: write_html(&dir),
            attachment_name: "index.html".to_string(),
        };
        send(&config, report).await.unwrap();

        let count = std::fs::read_dir(&outbox).unwrap().count();
        assert_eq!(count, 1, "one .eml should be written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
