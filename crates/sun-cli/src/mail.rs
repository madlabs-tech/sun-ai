//! [`SmtpMailer`]: the real [`Mailer`] (invites and the SMTP test), an
//! **Adapter** over `lettre` with rustls. Settings come per send, so an
//! admin's change applies to the next mail.

use std::time::Duration;

use async_trait::async_trait;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use sun_core::model::{Mail, SmtpSettings, SmtpTls};
use sun_core::{Error, Mailer, Result};

/// Gives up on a mail server that doesn't answer.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Sends through the ecosystem's SMTP server.
pub struct SmtpMailer;

fn message(smtp: &SmtpSettings, mail: &Mail) -> Result<Message> {
    let bad = |what: &str| Error::InvalidInput(format!("mail: {what} is not a valid address"));
    Message::builder()
        .from(smtp.from.parse().map_err(|_| bad("the From address"))?)
        .to(mail.to.parse().map_err(|_| bad("the recipient"))?)
        .subject(mail.subject.clone())
        .header(ContentType::TEXT_PLAIN)
        .body(mail.body.clone())
        .map_err(|e| Error::InvalidInput(format!("mail: {e}")))
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, smtp: &SmtpSettings, password: Option<&str>, mail: &Mail) -> Result<()> {
        let message = message(smtp, mail)?;
        let relay = match smtp.tls {
            SmtpTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host),
            SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host),
        }
        .map_err(|e| Error::Unavailable(format!("smtp {}: {e}", smtp.host)))?
        .port(smtp.port)
        .timeout(Some(TIMEOUT));
        let relay = match smtp.username.is_empty() {
            true => relay,
            false => relay.credentials(Credentials::new(
                smtp.username.clone(),
                password.unwrap_or_default().to_owned(),
            )),
        };
        // lettre's errors name the server's reply, never the credentials.
        relay
            .build()
            .send(message)
            .await
            .map_err(|e| Error::Unavailable(format!("smtp {}: {e}", smtp.host)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn smtp_message_needs_valid_addresses() {
        let smtp = SmtpSettings {
            host: "smtp.example.com".into(),
            port: 587,
            username: String::new(),
            from: "sun@example.com".into(),
            tls: SmtpTls::Starttls,
        };
        let mut mail = Mail {
            to: "ann@example.com".into(),
            subject: "Hi".into(),
            body: "code".into(),
        };
        assert!(message(&smtp, &mail).is_ok());
        mail.to = "not an address".into();
        assert!(matches!(message(&smtp, &mail), Err(Error::InvalidInput(_))));
    }
}
