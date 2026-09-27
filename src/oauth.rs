use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::{Client, StatusCode, Url};
use serde_json::{Value, json};

use crate::auth_store::{Session, Vault};
use crate::direct::{Credentials, Provider};

const CHATGPT_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const GROK_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
const MAX_BYTES: usize = 128 * 1024;
const MAX_EXPIRY: u64 = 366 * 24 * 60 * 60;

pub struct DeviceLogin {
    provider: Provider,
    verification_url: String,
    user_code: String,
    device_code: String,
    interval: Duration,
    next_poll: Instant,
    deadline: Instant,
    finished: bool,
}

impl DeviceLogin {
    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

pub enum Poll {
    Pending(Duration),
    SignedIn(Session),
}

pub struct AuthClient {
    provider: Provider,
    origin: Url,
}

impl AuthClient {
    pub fn new(provider: Provider) -> Result<Self> {
        Ok(Self {
            provider,
            origin: Url::parse(match provider {
                Provider::ChatGpt => "https://auth.openai.com/",
                Provider::Grok => "https://auth.x.ai/",
            })?,
        })
    }

    fn name(&self) -> &'static str {
        match self.provider {
            Provider::ChatGpt => "chatgpt",
            Provider::Grok => "grok",
        }
    }

    fn client_id(&self) -> &'static str {
        match self.provider {
            Provider::ChatGpt => CHATGPT_CLIENT,
            Provider::Grok => GROK_CLIENT,
        }
    }

    pub fn begin(&self, cancelled: impl Fn() -> bool) -> Result<DeviceLogin> {
        let (path, body) = match self.provider {
            Provider::ChatGpt => (
                "api/accounts/deviceauth/usercode",
                Body::Json(json!({"client_id":self.client_id()})),
            ),
            Provider::Grok => (
                "oauth2/device/code",
                Body::Form(vec![
                    ("client_id", self.client_id().into()),
                    ("scope", GROK_SCOPES.into()),
                    ("referrer", "aegis".into()),
                ]),
            ),
        };
        let started = Instant::now();
        let (status, value) = self.post(path, body, Duration::from_secs(30), &cancelled)?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        let user_code = match self.provider {
            Provider::ChatGpt => value.get("user_code").or_else(|| value.get("usercode")),
            Provider::Grok => value.get("user_code"),
        }
        .and_then(Value::as_str)
        .context("Sign-in service returned no verification code")?;
        if user_code.is_empty()
            || user_code.len() > 64
            || !user_code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("Sign-in service returned an invalid verification code");
        }
        let device_code = opaque(
            &value,
            match self.provider {
                Provider::ChatGpt => "device_auth_id",
                Provider::Grok => "device_code",
            },
        )?;
        let verification_url = match self.provider {
            Provider::ChatGpt => "https://auth.openai.com/codex/device".into(),
            Provider::Grok => {
                let url = value["verification_uri"]
                    .as_str()
                    .context("Sign-in service returned no verification URL")?;
                validate_verification_url(url)?;
                url.to_owned()
            }
        };
        let interval = match value.get("interval") {
            None => 5,
            Some(value) => number(value).context("Sign-in service returned an invalid interval")?,
        };
        let expiry = match self.provider {
            Provider::ChatGpt => 15 * 60,
            Provider::Grok => value["expires_in"]
                .as_u64()
                .context("Sign-in service returned no code expiration")?,
        };
        if !(1..=60).contains(&interval) || !(1..=15 * 60).contains(&expiry) {
            bail!("Sign-in timing exceeds the supported bounds");
        }
        let interval = Duration::from_secs(interval);
        let deadline = started + Duration::from_secs(expiry);
        if Instant::now() >= deadline || cancelled() {
            bail!("Sign-in code expired or was cancelled before presentation");
        }
        Ok(DeviceLogin {
            provider: self.provider,
            verification_url,
            user_code: user_code.into(),
            device_code,
            interval,
            next_poll: Instant::now() + interval,
            deadline,
            finished: false,
        })
    }

    pub fn poll(&self, login: &mut DeviceLogin, cancelled: impl Fn() -> bool) -> Result<Poll> {
        if login.provider != self.provider || login.finished {
            bail!("This sign-in attempt cannot be polled again");
        }
        if cancelled() || Instant::now() >= login.deadline {
            login.finished = true;
            bail!("Sign-in cancelled or verification code expired; start again");
        }
        if Instant::now() < login.next_poll {
            return Ok(Poll::Pending(
                login.next_poll.saturating_duration_since(Instant::now()),
            ));
        }
        let (path, body) = match self.provider {
            Provider::ChatGpt => (
                "api/accounts/deviceauth/token",
                Body::Json(json!({"device_auth_id":login.device_code,"user_code":login.user_code})),
            ),
            Provider::Grok => (
                "oauth2/token",
                Body::Form(vec![
                    ("client_id", self.client_id().into()),
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:device_code".into(),
                    ),
                    ("device_code", login.device_code.clone()),
                ]),
            ),
        };
        let result = self.post(
            path,
            body,
            login.expires_in().min(Duration::from_secs(30)),
            &cancelled,
        );
        let (status, value) = match result {
            Ok(result) => result,
            Err(error) => {
                login.finished = true;
                return Err(error);
            }
        };
        let pending = match self.provider {
            Provider::ChatGpt => status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            Provider::Grok => {
                status == StatusCode::BAD_REQUEST
                    && matches!(
                        value["error"].as_str(),
                        Some("authorization_pending" | "slow_down")
                    )
            }
        };
        if pending {
            if self.provider == Provider::Grok && value["error"] == "slow_down" {
                login.interval =
                    (login.interval + Duration::from_secs(5)).min(Duration::from_secs(15 * 60));
            }
            login.next_poll = Instant::now() + login.interval;
            return Ok(Poll::Pending(login.interval.min(login.expires_in())));
        }
        login.finished = true;
        if cancelled() || Instant::now() >= login.deadline {
            bail!("Sign-in cancelled or verification code expired before acceptance");
        }
        if !status.is_success() {
            return Err(status_error(status));
        }
        let tokens = match self.provider {
            Provider::Grok => value,
            Provider::ChatGpt => {
                let code = opaque(&value, "authorization_code")?;
                let verifier = opaque(&value, "code_verifier")?;
                let (status, tokens) = self.post(
                    "oauth/token",
                    Body::Form(vec![
                        ("grant_type", "authorization_code".into()),
                        ("client_id", self.client_id().into()),
                        (
                            "redirect_uri",
                            "https://auth.openai.com/deviceauth/callback".into(),
                        ),
                        ("code", code),
                        ("code_verifier", verifier),
                    ]),
                    login.expires_in().min(Duration::from_secs(30)),
                    &cancelled,
                )?;
                if !status.is_success() {
                    return Err(status_error(status));
                }
                tokens
            }
        };
        if cancelled() || Instant::now() >= login.deadline {
            bail!("Sign-in cancelled or expired before accepting credentials");
        }
        Ok(Poll::SignedIn(self.session(&tokens, None)?))
    }

    pub fn save(
        &self,
        vault: &Vault,
        session: &Session,
        cancelled: impl Fn() -> bool,
    ) -> Result<()> {
        if session.provider != self.name() {
            bail!("Sign-in does not belong to this provider");
        }
        let _lock = vault.lock(self.name(), Duration::from_secs(30), &cancelled)?;
        if cancelled() {
            bail!("Sign-in cancelled before saving credentials");
        }
        vault.save(session)
    }

    pub fn logout(&self, vault: &Vault, cancelled: impl Fn() -> bool) -> Result<bool> {
        let _lock = vault.lock(self.name(), Duration::from_secs(30), &cancelled)?;
        if cancelled() {
            bail!("Sign-out cancelled before removing credentials");
        }
        vault.remove(self.name())
    }

    pub fn credentials(&self, vault: &Vault, cancelled: impl Fn() -> bool) -> Result<Credentials> {
        let _lock = vault.lock(self.name(), Duration::from_secs(30), &cancelled)?;
        let mut session = vault
            .load(self.name())?
            .context("Not signed in to this provider; choose Sign in")?;
        let now = now()?;
        if session.expires_at > now.saturating_add(60)
            || (session.expires_at > now && session.refresh_token.is_none())
        {
            return session.credentials();
        }
        let refresh = session
            .refresh_token
            .take()
            .context("Saved sign-in expired; choose Sign in again")?;
        vault.save(&session)?;
        let (status, tokens) = self.post(
            match self.provider {
                Provider::ChatGpt => "oauth/token",
                Provider::Grok => "oauth2/token",
            },
            match self.provider {
                Provider::ChatGpt => Body::Json(json!({
                    "grant_type":"refresh_token", "client_id":self.client_id(),
                    "refresh_token":refresh,
                })),
                Provider::Grok => Body::Form(vec![
                    ("grant_type", "refresh_token".into()),
                    ("client_id", self.client_id().into()),
                    ("refresh_token", refresh.clone()),
                ]),
            },
            Duration::from_secs(30),
            &cancelled,
        )?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        let mut updated = self.session(&tokens, session.account_id.as_deref())?;
        if updated.refresh_token.is_none() {
            updated.refresh_token = Some(refresh);
        }
        if cancelled() {
            bail!("Refresh cancelled; sign in again if the saved session expires");
        }
        vault.save(&updated)?;
        updated.credentials()
    }

    fn session(&self, tokens: &Value, previous_account: Option<&str>) -> Result<Session> {
        let access_token = opaque(tokens, "access_token")?;
        let refresh_token = tokens
            .get("refresh_token")
            .filter(|value| !value.is_null())
            .map(|_| opaque(tokens, "refresh_token"))
            .transpose()?;
        if tokens
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            bail!("Sign-in service returned an unsupported token type");
        }
        let now = now()?;
        let (account_id, expires_at) = match self.provider {
            Provider::Grok => {
                let expiry = tokens["expires_in"]
                    .as_u64()
                    .filter(|expiry| (1..=MAX_EXPIRY).contains(expiry))
                    .context("Sign-in service returned an invalid token expiration")?;
                (
                    None,
                    now.checked_add(expiry).context("Token expiry overflow")?,
                )
            }
            Provider::ChatGpt => {
                let claims = jwt_claims(&access_token)?;
                let account = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
                    .as_str()
                    .context("ChatGPT access token has no selected account")?;
                if previous_account.is_some_and(|previous| previous != account) {
                    bail!("Refresh changed the selected ChatGPT account; sign in explicitly");
                }
                if let Some(id_token) = tokens["id_token"].as_str() {
                    let identity = jwt_claims(id_token)?;
                    if identity["https://api.openai.com/auth"]["chatgpt_account_id"] != account {
                        bail!("ChatGPT identity and access token accounts do not agree");
                    }
                }
                let expiry = claims["exp"]
                    .as_u64()
                    .filter(|expiry| *expiry > now && *expiry <= now.saturating_add(MAX_EXPIRY))
                    .context("ChatGPT access token expiration is missing or invalid")?;
                (Some(account.into()), expiry)
            }
        };
        let session = Session {
            provider: self.name().into(),
            access_token,
            refresh_token,
            account_id,
            expires_at,
        };
        session.credentials()?;
        Ok(session)
    }

    fn post(
        &self,
        path: &str,
        body: Body,
        timeout: Duration,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(StatusCode, Value)> {
        if timeout.is_zero() || cancelled() {
            bail!("Sign-in request cancelled before dispatch");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let client = Client::builder()
                .timeout(timeout)
                .connect_timeout(timeout.min(Duration::from_secs(10)))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("aegis/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|_| anyhow!("Could not initialize secure sign-in transport"))?;
            let mut request = client
                .post(self.origin.join(path)?)
                .header("Accept", "application/json");
            if self.provider == Provider::Grok {
                request = request
                    .header("x-grok-client-version", crate::direct::GROK_REFERENCE_TRANSPORT_VERSION)
                    .header("x-grok-client-surface", "ui")
                    .header("x-grok-client-identifier", "aegis")
                    .header("x-aegis-client-version", env!("CARGO_PKG_VERSION"));
            }
            request = match body {
                Body::Json(value) => request.json(&value),
                Body::Form(fields) => {
                    let mut encoded = self.origin.clone();
                    encoded.query_pairs_mut().extend_pairs(&fields);
                    request
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .body(encoded.query().unwrap_or("").to_owned())
                }
            };
            let fetch = async {
                let mut response = request
                    .send()
                    .await
                    .map_err(|_| anyhow!("Sign-in connection failed; this request was not retried"))?;
                let status = response.status();
                if response.content_length().is_some_and(|size| size > MAX_BYTES as u64) {
                    bail!("Sign-in response exceeds its byte bound");
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await
                    .map_err(|_| anyhow!("Sign-in response interrupted; this request was not retried"))? {
                    if bytes.len() + chunk.len() > MAX_BYTES {
                        bytes.fill(0);
                        bail!("Sign-in response exceeds its byte bound");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let parsed = serde_json::from_slice::<Value>(&bytes);
                bytes.fill(0);
                let value = if status.is_success() {
                    parsed.map_err(|_| anyhow!("Sign-in service returned invalid JSON"))?
                } else {
                    parsed.unwrap_or(Value::Null)
                };
                Ok::<_, anyhow::Error>((status, value))
            };
            tokio::pin!(fetch);
            loop {
                tokio::select! {
                    result = &mut fetch => {
                        if cancelled() { bail!("Sign-in request cancelled before acceptance"); }
                        break result;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {
                        if cancelled() { bail!("Sign-in request cancelled; this request was not retried"); }
                    }
                }
            }
        })
    }
}

enum Body {
    Json(Value),
    Form(Vec<(&'static str, String)>),
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn opaque(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 32768
                && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .map(str::to_owned)
        .context("Sign-in response contains an invalid required credential")
}

fn validate_verification_url(value: &str) -> Result<()> {
    if value.len() > 2048 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("Sign-in service returned an unsafe verification URL");
    }
    let url = Url::parse(value).map_err(|_| anyhow!("Invalid sign-in verification URL"))?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("auth.x.ai" | "accounts.x.ai"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
    {
        bail!("Sign-in verification URL is outside the trusted provider");
    }
    Ok(())
}

fn jwt_claims(token: &str) -> Result<Value> {
    let fields = token.split('.').collect::<Vec<_>>();
    if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
        bail!("ChatGPT token metadata has an invalid format");
    }
    let mut bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(fields[1])
        .map_err(|_| anyhow!("ChatGPT token metadata is invalid"))?;
    let claims = serde_json::from_slice(&bytes);
    bytes.fill(0);
    claims.map_err(|_| anyhow!("ChatGPT token metadata is invalid"))
}

fn status_error(status: StatusCode) -> anyhow::Error {
    anyhow!(match status.as_u16() {
        400 | 401 => "Sign-in was rejected or expired; start sign-in again",
        403 => "Provider does not permit this sign-in; check account access",
        404 => "Device sign-in is unavailable for this account or provider",
        429 => "Too many sign-in requests; wait before trying again",
        300..=399 => "Sign-in redirect was refused to protect credentials",
        _ => "Sign-in service is unavailable; try again later",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    struct Reply {
        status: &'static str,
        headers: String,
        body: String,
        delay: Duration,
    }

    fn reply(status: &'static str, value: Value) -> Reply {
        let body = value.to_string();
        Reply {
            status,
            headers: format!("Content-Length: {}\r\n", body.len()),
            body,
            delay: Duration::ZERO,
        }
    }

    fn fixture(
        provider: Provider,
        replies: Vec<Reply>,
    ) -> Result<(AuthClient, std::thread::JoinHandle<Result<Vec<String>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let origin = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        let worker = std::thread::spawn(move || -> Result<Vec<String>> {
            let mut requests = Vec::new();
            for response in replies {
                let started = Instant::now();
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && started.elapsed() < Duration::from_secs(5) =>
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer)?;
                    if count == 0 || bytes.len() + count > 128 * 1024 {
                        bail!("OAuth fixture request was incomplete or oversized");
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..end])?;
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .context("OAuth fixture request has no content length")?;
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes)?);
                std::thread::sleep(response.delay);
                let header = format!(
                    "HTTP/1.1 {}\r\n{}Connection: close\r\n\r\n",
                    response.status, response.headers
                );
                if stream.write_all(header.as_bytes()).is_ok() {
                    let _ = stream.write_all(response.body.as_bytes());
                }
            }
            Ok(requests)
        });
        Ok((AuthClient { provider, origin }, worker))
    }

    fn jwt(account: &str, expiry: u64) -> String {
        let payload =
            json!({"exp":expiry,"https://api.openai.com/auth":{"chatgpt_account_id":account}});
        format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    fn tokens(provider: Provider, account: &str) -> Result<Value> {
        Ok(match provider {
            Provider::ChatGpt => {
                json!({"access_token":jwt(account,now()?+3600),"id_token":jwt(account,now()?+3600),"refresh_token":"rotated-private-refresh"})
            }
            Provider::Grok => {
                json!({"access_token":"new-private-access","refresh_token":"rotated-private-refresh","expires_in":3600,"token_type":"Bearer"})
            }
        })
    }

    fn code(provider: Provider) -> Value {
        match provider {
            Provider::ChatGpt => {
                json!({"device_auth_id":"private-device-id","user_code":"ABCD-1234","interval":"1"})
            }
            Provider::Grok => {
                json!({"device_code":"private-device-id","user_code":"ABCD-1234","verification_uri":"https://accounts.x.ai/device","expires_in":300,"interval":1})
            }
        }
    }

    #[test]
    fn both_device_protocols_use_owned_identity_and_single_use_exchange() -> Result<()> {
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let mut replies = vec![reply("200 OK", code(provider))];
            if provider == Provider::ChatGpt {
                replies.push(reply("200 OK", json!({"authorization_code":"private-authorization-code","code_verifier":"private-verifier"})));
            }
            replies.push(reply("200 OK", tokens(provider, "selected-account")?));
            let (client, worker) = fixture(provider, replies)?;
            let mut login = client.begin(|| false)?;
            assert_eq!(login.user_code(), "ABCD-1234");
            assert!(login.expires_in() > Duration::ZERO);
            assert!(matches!(
                client.poll(&mut login, || false)?,
                Poll::Pending(_)
            ));
            login.next_poll = Instant::now();
            let Poll::SignedIn(session) = client.poll(&mut login, || false)? else {
                bail!("Fixture did not finish sign-in");
            };
            assert!(session.credentials().is_ok());
            assert_eq!(
                session.account_id.as_deref(),
                (provider == Provider::ChatGpt).then_some("selected-account")
            );
            assert!(client.poll(&mut login, || false).is_err());
            let requests = worker.join().unwrap()?;
            assert_eq!(
                requests.len(),
                if provider == Provider::ChatGpt { 3 } else { 2 }
            );
            assert!(
                requests
                    .iter()
                    .all(|request| request.to_lowercase().contains("user-agent: aegis/"))
            );
            assert!(
                requests
                    .iter()
                    .all(|request| !request.contains("Authorization: Bearer"))
            );
            if provider == Provider::ChatGpt {
                assert!(requests[0].starts_with("POST /api/accounts/deviceauth/usercode "));
                assert!(requests[1].starts_with("POST /api/accounts/deviceauth/token "));
                assert!(requests[2].starts_with("POST /oauth/token "));
                assert!(requests[2].contains("grant_type=authorization_code"));
                assert!(requests[2].contains("code_verifier=private-verifier"));
            } else {
                assert!(requests[0].starts_with("POST /oauth2/device/code "));
                assert!(requests[0].contains("referrer=aegis"));
                assert!(!requests[0].contains("conversations%3Awrite"));
                assert!(!requests[0].contains("workspaces%3Awrite"));
                assert!(
                    requests[1].contains(
                        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
                    )
                );
            }
        }
        Ok(())
    }

    #[test]
    fn pending_slowdown_denial_expiry_and_cancellation_are_not_retried_blindly() -> Result<()> {
        let (client, worker) = fixture(
            Provider::Grok,
            vec![
                reply("200 OK", code(Provider::Grok)),
                reply("400 Bad Request", json!({"error":"authorization_pending"})),
                reply("400 Bad Request", json!({"error":"slow_down"})),
                reply(
                    "403 Forbidden",
                    json!({"error":"private-secret-error","error_description":"private-secret-description"}),
                ),
            ],
        )?;
        let mut login = client.begin(|| false)?;
        for expected in [1, 6] {
            login.next_poll = Instant::now();
            let Poll::Pending(delay) = client.poll(&mut login, || false)? else {
                bail!("Expected a pending code");
            };
            assert_eq!(delay, Duration::from_secs(expected));
            assert!(login.next_poll > Instant::now());
        }
        login.next_poll = Instant::now();
        let error = client.poll(&mut login, || false).err().unwrap().to_string();
        assert!(!error.contains("private-secret"));
        assert!(client.poll(&mut login, || false).is_err());
        assert_eq!(worker.join().unwrap()?.len(), 4);
        for cancel in [true, false] {
            let (client, worker) =
                fixture(Provider::Grok, vec![reply("200 OK", code(Provider::Grok))])?;
            let mut login = client.begin(|| false)?;
            if !cancel {
                login.deadline = Instant::now();
            }
            assert!(client.poll(&mut login, || cancel).is_err());
            assert!(login.finished);
            assert_eq!(worker.join().unwrap()?.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn verification_codes_urls_and_timing_cannot_inject_ui_or_external_navigation() -> Result<()> {
        for url in [
            "http://accounts.x.ai/device",
            "https://evil.test/device",
            "https://accounts.x.ai.evil.test/device",
            "https://name@accounts.x.ai/device",
            "https://accounts.x.ai:444/device",
            "https://accounts.x.ai/device#external",
            "https://accounts.x.ai/device\n",
            "file:///auth",
        ] {
            assert!(validate_verification_url(url).is_err());
        }
        assert!(validate_verification_url("https://accounts.x.ai/device").is_ok());
        for (field, value) in [
            ("user_code", json!("\u{1b}[2J")),
            ("user_code", json!("")),
            ("interval", json!(0)),
            ("interval", json!(61)),
            ("expires_in", json!(0)),
            ("expires_in", json!(901)),
            ("device_code", json!("private secret")),
            ("verification_uri", json!("https://evil.test/")),
        ] {
            let mut response = code(Provider::Grok);
            response[field] = value;
            let (client, worker) = fixture(Provider::Grok, vec![reply("200 OK", response)])?;
            assert!(client.begin(|| false).is_err());
            assert_eq!(worker.join().unwrap()?.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn token_metadata_never_silently_changes_account_or_accepts_invalid_expiry() -> Result<()> {
        let client = AuthClient::new(Provider::ChatGpt)?;
        assert!(
            client
                .session(
                    &tokens(Provider::ChatGpt, "other-account")?,
                    Some("selected-account")
                )
                .is_err()
        );
        let mut value = tokens(Provider::ChatGpt, "selected-account")?;
        value["id_token"] = json!(jwt("other-account", now()? + 3600));
        assert!(client.session(&value, None).is_err());
        value["id_token"] = Value::Null;
        value["access_token"] = json!(jwt("selected-account", now()?));
        assert!(client.session(&value, None).is_err());
        assert!(jwt_claims("private-secret-invalid-token").is_err());
        let client = AuthClient::new(Provider::Grok)?;
        for expiry in [0, MAX_EXPIRY + 1] {
            let mut value = tokens(Provider::Grok, "")?;
            value["expires_in"] = json!(expiry);
            assert!(client.session(&value, None).is_err());
        }
        let mut value = tokens(Provider::Grok, "")?;
        value["refresh_token"] = json!("private secret");
        assert!(client.session(&value, None).is_err());
        Ok(())
    }

    #[test]
    fn refresh_rotation_is_saved_and_fresh_sessions_do_not_call_the_network() -> Result<()> {
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let directory = tempfile::tempdir()?;
            let vault = Vault::new(directory.path().join("auth"));
            let mut initial =
                AuthClient::new(provider)?.session(&tokens(provider, "selected-account")?, None)?;
            initial.expires_at = now()? - 1;
            initial.refresh_token = Some("old-private-refresh+/=".into());
            vault.save(&initial)?;
            let (client, worker) = fixture(
                provider,
                vec![reply("200 OK", tokens(provider, "selected-account")?)],
            )?;
            client.credentials(&vault, || false)?;
            assert_eq!(
                vault.load(client.name())?.unwrap().refresh_token.as_deref(),
                Some("rotated-private-refresh")
            );
            client.credentials(&vault, || false)?;
            let requests = worker.join().unwrap()?;
            assert_eq!(requests.len(), 1);
            match provider {
                Provider::ChatGpt => {
                    let body: Value =
                        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1)?;
                    assert_eq!(body["grant_type"], "refresh_token");
                    assert_eq!(body["refresh_token"], "old-private-refresh+/=");
                    assert!(
                        requests[0]
                            .to_lowercase()
                            .contains("content-type: application/json")
                    );
                }
                Provider::Grok => {
                    assert!(requests[0].contains("grant_type=refresh_token"));
                    assert!(requests[0].contains("refresh_token=old-private-refresh%2B%2F%3D"));
                }
            }
            assert!(client.logout(&vault, || false)?);
            assert!(vault.load(client.name())?.is_none());
        }
        Ok(())
    }

    #[test]
    fn rejected_refresh_keeps_unknown_consumption_durable_and_does_not_replay_old_token()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let mut initial =
            AuthClient::new(Provider::Grok)?.session(&tokens(Provider::Grok, "")?, None)?;
        initial.expires_at = now()? - 1;
        vault.save(&initial)?;
        let (client, worker) = fixture(
            Provider::Grok,
            vec![reply(
                "400 Bad Request",
                json!({"error_description":"rotated-private-refresh"}),
            )],
        )?;
        assert!(client.credentials(&vault, || false).is_err());
        assert!(vault.load("grok")?.unwrap().refresh_token.is_none());
        let error = client
            .credentials(&vault, || false)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("rotated-private-refresh"));
        assert_eq!(worker.join().unwrap()?.len(), 1);
        Ok(())
    }

    #[test]
    fn concurrent_callers_refresh_once_then_reload_the_rotated_session() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("auth");
        let vault = Vault::new(root.clone());
        let mut initial =
            AuthClient::new(Provider::Grok)?.session(&tokens(Provider::Grok, "")?, None)?;
        initial.expires_at = now()? - 1;
        vault.save(&initial)?;
        let mut response = reply("200 OK", tokens(Provider::Grok, "")?);
        response.delay = Duration::from_millis(100);
        let (client, worker) = fixture(Provider::Grok, vec![response])?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let callers = (0..2)
            .map(|_| {
                let root = root.clone();
                let origin = client.origin.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || -> Result<()> {
                    barrier.wait();
                    AuthClient {
                        provider: Provider::Grok,
                        origin,
                    }
                    .credentials(&Vault::new(root), || false)?;
                    Ok(())
                })
            })
            .collect::<Vec<_>>();
        for caller in callers {
            caller.join().unwrap()?;
        }
        assert_eq!(worker.join().unwrap()?.len(), 1);
        assert_eq!(
            vault.load("grok")?.unwrap().refresh_token.as_deref(),
            Some("rotated-private-refresh")
        );
        Ok(())
    }

    #[test]
    fn auth_locks_are_provider_specific_bounded_cancellable_and_release_on_drop() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let lock = vault.lock("chatgpt", Duration::from_secs(1), &|| false)?;
        assert!(
            vault
                .lock("chatgpt", Duration::from_millis(30), &|| false)
                .is_err()
        );
        let other = vault.lock("grok", Duration::from_secs(1), &|| false)?;
        drop(other);
        drop(lock);
        let lock = vault.lock("chatgpt", Duration::from_secs(1), &|| false)?;
        drop(lock);
        assert!(
            vault
                .lock("grok", Duration::from_secs(1), &|| true)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn auth_transport_bounds_redirects_chunked_bodies_and_inflight_cancellation() -> Result<()> {
        let replies = [
            Reply {
                status: "302 Found",
                headers: "Location: https://evil.test/token\r\nContent-Length: 0\r\n".into(),
                body: String::new(),
                delay: Duration::ZERO,
            },
            Reply {
                status: "200 OK",
                headers: format!("Content-Length: {}\r\n", MAX_BYTES + 1),
                body: String::new(),
                delay: Duration::ZERO,
            },
            Reply {
                status: "200 OK",
                headers: "Transfer-Encoding: chunked\r\n".into(),
                body: format!(
                    "{:x}\r\n{}\r\n0\r\n\r\n",
                    MAX_BYTES + 1,
                    "x".repeat(MAX_BYTES + 1)
                ),
                delay: Duration::ZERO,
            },
        ];
        for response in replies {
            let (client, worker) = fixture(Provider::Grok, vec![response])?;
            assert!(client.begin(|| false).is_err());
            assert_eq!(worker.join().unwrap()?.len(), 1);
        }
        let mut response = reply("200 OK", code(Provider::Grok));
        response.delay = Duration::from_millis(250);
        let (client, worker) = fixture(Provider::Grok, vec![response])?;
        let started = Instant::now();
        assert!(
            client
                .begin(|| started.elapsed() >= Duration::from_millis(100))
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(230));
        assert_eq!(worker.join().unwrap()?.len(), 1);
        Ok(())
    }
}
