use std::time::Duration;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::api::{WeixinApi, WeixinError};
use crate::types::{Credentials, LoginPoll, QrChallenge};

pub async fn login<A, F, V>(
    api: &A,
    cancel: CancellationToken,
    mut show_qr: F,
    mut verify: V,
) -> Result<Credentials, WeixinError>
where
    A: WeixinApi,
    F: FnMut(&QrChallenge) -> Result<(), WeixinError>,
    V: FnMut() -> Result<String, WeixinError>,
{
    let result = timeout(Duration::from_secs(300), async {
        let mut challenge = api.begin_login().await?;
        show_qr(&challenge)?;
        let mut verify_attempts = 0;
        let mut redirects = 0;
        let mut failures = 0;
        let mut verify_code = None;
        loop {
            if cancel.is_cancelled() { return Err(WeixinError::Cancelled); }
            let poll = match api.poll_login(&challenge, verify_code.as_deref()).await {
                Ok(poll) => {
                    failures = 0;
                    poll
                }
                Err(error)
                    if matches!(&error, WeixinError::Network(_))
                        || matches!(&error, WeixinError::Http(status) if status.as_u16() == 429 || status.is_server_error()) =>
                {
                    failures += 1;
                    if failures > 5 {
                        return Err(error);
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(WeixinError::Cancelled),
                        _ = sleep(Duration::from_millis(250 * failures)) => {}
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            match poll {
                LoginPoll::Wait => {}
                LoginPoll::Scanned => { verify_code = None; }
                LoginPoll::NeedVerifyCode => {
                    verify_attempts += 1;
                    if verify_attempts > 3 {
                        return Err(WeixinError::Protocol("verification attempt limit reached".into()));
                    }
                    let code = verify()?;
                    if code.is_empty() || code.len() > 12 || !code.chars().all(|character| character.is_ascii_digit()) {
                        return Err(WeixinError::Protocol("verification code must contain 1-12 digits".into()));
                    }
                    verify_code = Some(code);
                }
                LoginPoll::Redirect { host } => {
                    redirects += 1;
                    if redirects > 3 {
                        return Err(WeixinError::Protocol("QR redirect limit reached".into()));
                    }
                    api.redirect_base(&host).await?;
                    challenge.base_url = format!("https://{host}");
                }
                LoginPoll::Confirmed(credentials) => return Ok(credentials),
                LoginPoll::Expired => return Err(WeixinError::Protocol("QR expired".into())),
                LoginPoll::VerifyCodeBlocked => return Err(WeixinError::Protocol("verification blocked".into())),
                LoginPoll::BindedRedirect => return Err(WeixinError::Protocol("existing binding returned no credentials".into())),
            }
            tokio::select! { _ = cancel.cancelled() => return Err(WeixinError::Cancelled), _ = sleep(Duration::from_secs(1)) => {} }
        }
    }).await;
    result.map_err(|_| WeixinError::Protocol("QR login timed out".into()))?
}
