//! Mirrors `server.js`'s email helpers: `escapeHtml`, `buildEmailHtml`,
//! `sendVerificationEmail`, `sendPasswordResetEmail`, and the
//! `mailTransport` (nodemailer) setup. SMTP is optional — with no
//! `SMTP_HOST` configured, the link is logged instead of emailed (the same
//! fallback Node uses for local/self-hosted dev), and `EXPOSE_DEV_VERIFY_URL`
//! controls whether callers also see it in the response body.

use lettre::message::header::ContentType;
use lettre::message::{MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct Mailer {
    transport: Option<AsyncSmtpTransport<Tokio1Executor>>,
    from: String,
}

impl Mailer {
    /// Mirrors the `mailTransport` construction: `SMTP_HOST` present is
    /// what gates whether email actually sends at all; port 465 implies
    /// implicit TLS ("secure"), matching nodemailer's own convention.
    pub fn from_env() -> Self {
        let from = std::env::var("SMTP_FROM")
            .unwrap_or_else(|_| "AgentRaaS <no-reply@agentraas.local>".to_string());

        let Ok(host) = std::env::var("SMTP_HOST") else {
            return Self {
                transport: None,
                from,
            };
        };

        let port: u16 = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        let implicit_tls = port == 465;

        let mut builder = if implicit_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
        }
        .expect("invalid SMTP_HOST")
        .port(port);

        if let Ok(user) = std::env::var("SMTP_USER") {
            let pass = std::env::var("SMTP_PASS").unwrap_or_default();
            builder = builder.credentials(Credentials::new(user, pass));
        }

        Self {
            transport: Some(builder.build()),
            from,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.transport.is_some()
    }

    #[cfg(feature = "enterprise")]
    pub async fn send_org_invite_email(&self, to_email: &str, org_id: &str, role: &str, accept_url: &str) {
        if !self.is_configured() {
            tracing::warn!(to_email, org_id, role, accept_url, "SMTP not configured — org invite link");
            return;
        }
        let text = format!(
            "You've been invited to join the \"{org_id}\" org on AgentRaaS as {role}.\n\nAccept here: {accept_url}\n\nThis link expires in 7 days."
        );
        let html = build_email_html(EmailTemplate {
            heading: "You&rsquo;ve been invited",
            body_html: &format!(
                "You've been invited to join <strong>{}</strong> on AgentRaaS as <strong>{}</strong>.",
                escape_html(org_id),
                escape_html(role)
            ),
            cta_text: "Accept invite",
            cta_url: accept_url,
            expiry_note: Some("This link expires in 7 days."),
        });
        self.send(to_email, &format!("You've been invited to join {org_id} on AgentRaaS"), &text, &html).await;
    }

    async fn send(&self, to: &str, subject: &str, text: &str, html: &str) {
        let Some(transport) = &self.transport else {
            return;
        };
        let message = match Message::builder()
            .from(self.from.parse().expect("SMTP_FROM must be a valid address"))
            .to(match to.parse() {
                Ok(addr) => addr,
                Err(err) => {
                    tracing::error!(?err, to, "invalid recipient address");
                    return;
                }
            })
            .subject(subject)
            .multipart(MultiPart::alternative().singlepart(
                SinglePart::builder().header(ContentType::TEXT_PLAIN).body(text.to_string()),
            ).singlepart(
                SinglePart::builder().header(ContentType::TEXT_HTML).body(html.to_string()),
            )) {
            Ok(m) => m,
            Err(err) => {
                tracing::error!(?err, "failed to build email message");
                return;
            }
        };

        if let Err(err) = transport.send(message).await {
            tracing::error!(?err, "failed to send email");
        }
    }

    pub async fn send_verification_email(&self, to_email: &str, verify_url: &str) {
        if !self.is_configured() {
            tracing::warn!(to_email, verify_url, "SMTP not configured — email verification link");
            return;
        }
        let text = format!(
            "Welcome to AgentRaaS! Verify your email to activate your account.\n\nVerify here: {verify_url}\n\nThis link expires in 24 hours. If you didn't create this account, you can safely ignore this email."
        );
        let html = build_email_html(EmailTemplate {
            heading: "Verify your email",
            body_html: &format!(
                "Welcome to AgentRaaS — the exactly-once execution layer for AI agents. Click the button below to verify <strong>{}</strong> and activate your account.",
                escape_html(to_email)
            ),
            cta_text: "Verify email",
            cta_url: verify_url,
            expiry_note: Some("This link expires in 24 hours."),
        });
        self.send(to_email, "Verify your AgentRaaS account", &text, &html).await;
    }

    pub async fn send_password_reset_email(&self, to_email: &str, reset_url: &str) {
        if !self.is_configured() {
            tracing::warn!(to_email, reset_url, "SMTP not configured — password reset link");
            return;
        }
        let text = format!(
            "We received a request to reset your AgentRaaS password.\n\nReset it here: {reset_url}\n\nThis link expires in 1 hour and works once. If you didn't request this, you can safely ignore this email."
        );
        let html = build_email_html(EmailTemplate {
            heading: "Reset your password",
            body_html: &format!(
                "We received a request to reset the password on your AgentRaaS account (<strong>{}</strong>). Click the button below to choose a new one.",
                escape_html(to_email)
            ),
            cta_text: "Reset password",
            cta_url: reset_url,
            expiry_note: Some("This link expires in 1 hour and works once."),
        });
        self.send(to_email, "Reset your AgentRaaS password", &text, &html).await;
    }
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

struct EmailTemplate<'a> {
    heading: &'a str,
    body_html: &'a str,
    cta_text: &'a str,
    cta_url: &'a str,
    expiry_note: Option<&'a str>,
}

/// Byte-for-byte the same table-based inline-styled layout as
/// `buildEmailHtml()` in server.js (see that function's own comment for why:
/// many mail clients strip `<style>` blocks / don't support flexbox/grid).
fn build_email_html(t: EmailTemplate) -> String {
    let expiry_html = t
        .expiry_note
        .map(|note| format!(r#"<p style="font-size:13px;color:#8B93A6;margin-top:16px;">{note}</p>"#))
        .unwrap_or_default();

    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0;padding:0;background:#F3F1EC;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#F3F1EC;padding:40px 20px;">
<tr><td align="center">
<table role="presentation" width="480" cellpadding="0" cellspacing="0" style="background:#FFFFFF;border-radius:14px;overflow:hidden;max-width:480px;width:100%;">
  <tr><td style="background:#0A0D14;padding:24px 32px;">
    <table role="presentation" cellpadding="0" cellspacing="0"><tr>
      <td style="width:28px;height:28px;background:#00E0A8;border-radius:8px;text-align:center;vertical-align:middle;font-size:14px;">🛡️</td>
      <td style="padding-left:10px;color:#FFFFFF;font-size:17px;font-weight:700;">AgentRaaS</td>
    </tr></table>
  </td></tr>
  <tr><td style="padding:36px 32px 28px;">
    <h1 style="margin:0 0 16px;font-size:21px;color:#14171F;">{heading}</h1>
    <div style="font-size:15px;line-height:1.6;color:#3A4152;margin-bottom:28px;">{body_html}</div>
    <table role="presentation" cellpadding="0" cellspacing="0"><tr>
      <td style="background:#00E0A8;border-radius:999px;">
        <a href="{cta_url}" style="display:inline-block;padding:13px 28px;color:#06231C;font-weight:700;font-size:15px;text-decoration:none;">{cta_text}</a>
      </td>
    </tr></table>
    <p style="font-size:13px;color:#8B93A6;margin-top:24px;line-height:1.5;">
      Or copy and paste this link into your browser:<br>
      <a href="{cta_url}" style="color:#059669;word-break:break-all;">{cta_url}</a>
    </p>
    {expiry_html}
  </td></tr>
  <tr><td style="padding:20px 32px;background:#F9F8F5;border-top:1px solid #E4E0D6;">
    <p style="font-size:12.5px;color:#8B93A6;margin:0;line-height:1.5;">
      If you didn't request this, you can safely ignore this email.
      Questions? Reach us at <a href="mailto:support@agentraas.io" style="color:#059669;">support@agentraas.io</a>.
    </p>
  </td></tr>
</table>
</td></tr>
</table>
</body></html>"#,
        heading = t.heading,
        body_html = t.body_html,
        cta_url = t.cta_url,
        cta_text = t.cta_text,
        expiry_html = expiry_html,
    )
}
