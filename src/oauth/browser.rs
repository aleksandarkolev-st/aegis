use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256, Sha384, Sha512};

use super::*;

const CALLBACK_BYTES: usize = 8192;

pub struct BrowserLogin {
    provider: Provider,
    listener: Option<TcpListener>,
    authorization_url: String,
    redirect_uri: Url,
    token_path: String,
    verifier: String,
    state: String,
    nonce: String,
    deadline: Instant,
    finished: bool,
}

impl BrowserLogin {
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn end(&mut self) {
        self.finished = true;
        self.listener = None;
    }
}

impl Drop for BrowserLogin {
    fn drop(&mut self) {
        for secret in [&mut self.verifier, &mut self.state, &mut self.nonce] {
            let mut bytes = std::mem::take(secret).into_bytes();
            bytes.fill(0);
        }
    }
}

impl AuthClient {
    pub fn begin_browser(&self, cancelled: impl Fn() -> bool) -> Result<BrowserLogin> {
        let ports: &[u16] = match self.provider {
            Provider::ChatGpt => &[1455, 1457],
            Provider::Grok => &[0],
        };
        self.begin_browser_on(ports, &cancelled)
    }

    fn begin_browser_on(
        &self,
        ports: &[u16],
        cancelled: &impl Fn() -> bool,
    ) -> Result<BrowserLogin> {
        if cancelled() {
            bail!("Browser sign-in cancelled before startup");
        }
        let started = Instant::now();
        let listener = ports
            .iter()
            .find_map(|port| TcpListener::bind(("127.0.0.1", *port)).ok())
            .context(
                "Browser callback ports are unavailable; choose device-code sign-in instead",
            )?;
        listener.set_nonblocking(true)?;
        let callback = match self.provider {
            Provider::ChatGpt => "/auth/callback",
            Provider::Grok => "/callback",
        };
        let redirect_uri = Url::parse(&format!(
            "http://127.0.0.1:{}{callback}",
            listener.local_addr()?.port()
        ))?;
        let (mut authorize, token_path) = match self.provider {
            Provider::ChatGpt => (
                self.origin.join("oauth/authorize")?,
                "oauth/token".to_owned(),
            ),
            Provider::Grok => {
                let (status, discovery) = self.post(
                    ".well-known/openid-configuration",
                    Body::Get,
                    Duration::from_secs(30),
                    cancelled,
                )?;
                if !status.is_success() {
                    return Err(status_error(status));
                }
                if discovery["issuer"] != self.origin.as_str().trim_end_matches('/') {
                    bail!("Browser sign-in discovery returned a different issuer");
                }
                let authorization =
                    trusted_endpoint(&self.origin, &discovery, "authorization_endpoint")?;
                let token = trusted_endpoint(&self.origin, &discovery, "token_endpoint")?;
                (
                    authorization,
                    token.path().trim_start_matches('/').to_owned(),
                )
            }
        };
        let verifier = random_secret();
        let state = random_secret();
        let nonce = random_secret();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        {
            let mut query = authorize.query_pairs_mut();
            query.extend_pairs([
                ("response_type", "code"),
                ("client_id", self.client_id()),
                ("redirect_uri", redirect_uri.as_str()),
                (
                    "scope",
                    match self.provider {
                        Provider::ChatGpt => "openid profile email offline_access",
                        Provider::Grok => GROK_SCOPES,
                    },
                ),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", state.as_str()),
                ("nonce", nonce.as_str()),
            ]);
            match self.provider {
                Provider::ChatGpt => {
                    query.extend_pairs([
                        ("originator", "aegis"),
                        ("id_token_add_organizations", "true"),
                    ]);
                }
                Provider::Grok => {
                    query.append_pair("referrer", "aegis");
                }
            }
        }
        if cancelled() {
            bail!("Browser sign-in cancelled before presentation");
        }
        Ok(BrowserLogin {
            provider: self.provider,
            listener: Some(listener),
            authorization_url: authorize.into(),
            redirect_uri,
            token_path,
            verifier,
            state,
            nonce,
            deadline: started + Duration::from_secs(LOGIN_WINDOW_SECONDS),
            finished: false,
        })
    }

    pub fn poll_browser(
        &self,
        login: &mut BrowserLogin,
        cancelled: impl Fn() -> bool,
    ) -> Result<Poll> {
        if login.provider != self.provider || login.finished {
            bail!("This browser sign-in attempt cannot be used again");
        }
        if cancelled() || login.expires_in().is_zero() {
            login.end();
            bail!("Browser sign-in cancelled or expired; start again");
        }
        let (mut stream, peer) = match login
            .listener
            .as_ref()
            .context("Browser callback listener closed")?
            .accept()
        {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(Poll::Pending(Duration::from_millis(25)));
            }
            Err(_) => {
                login.end();
                bail!("Browser callback listener failed; start again");
            }
        };
        if !peer.ip().is_loopback() {
            return Ok(Poll::Pending(Duration::from_millis(25)));
        }
        let callback = read_callback(&mut stream, login, &cancelled);
        if cancelled() || login.expires_in().is_zero() {
            login.end();
            bail!("Browser sign-in cancelled or expired before callback acceptance");
        }
        let fields = match callback {
            Ok(fields) => fields,
            Err(_) => {
                respond(
                    &mut stream,
                    400,
                    "This callback was not accepted. Return to Aegis and use the browser link from this sign-in.",
                );
                return Ok(Poll::Pending(Duration::from_millis(25)));
            }
        };
        login.end();
        if fields.contains_key("error") {
            respond(
                &mut stream,
                400,
                "Sign-in was not approved. Return to Aegis to try again.",
            );
            bail!("Browser sign-in was denied by the provider; start again");
        }
        let code = fields
            .get("code")
            .context("Browser callback contains no authorization code")?;
        respond(
            &mut stream,
            200,
            "Aegis received your browser callback and is verifying sign-in. Return to your terminal for the result.",
        );
        let (status, tokens) = self.post(
            &login.token_path,
            Body::Form(vec![
                ("grant_type", "authorization_code".into()),
                ("client_id", self.client_id().into()),
                ("redirect_uri", login.redirect_uri.as_str().into()),
                ("code", code.clone()),
                ("code_verifier", login.verifier.clone()),
            ]),
            login.expires_in().min(Duration::from_secs(30)),
            &cancelled,
        )?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        validate_identity(
            &tokens,
            login,
            self.origin.as_str().trim_end_matches('/'),
            self.client_id(),
            code,
        )?;
        if cancelled() || login.expires_in().is_zero() {
            bail!("Browser sign-in cancelled or expired before credential acceptance");
        }
        Ok(Poll::SignedIn(self.session(&tokens, None)?))
    }
}

fn random_secret() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let secret = URL_SAFE_NO_PAD.encode(&bytes);
    bytes.fill(0);
    secret
}

fn trusted_endpoint(origin: &Url, discovery: &Value, field: &str) -> Result<Url> {
    let endpoint = discovery[field]
        .as_str()
        .filter(|value| value.len() <= 2048)
        .context("Browser sign-in discovery is incomplete")?;
    let url = Url::parse(endpoint)
        .map_err(|_| anyhow!("Browser sign-in discovery returned an invalid endpoint"))?;
    if url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() == "/"
    {
        bail!("Browser sign-in discovery endpoint is outside the trusted issuer");
    }
    Ok(url)
}

fn equal_secret(expected: &str, actual: &str) -> bool {
    expected.len() == actual.len()
        && expected
            .bytes()
            .zip(actual.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn read_callback(
    stream: &mut TcpStream,
    login: &BrowserLogin,
    cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, String>> {
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + login.expires_in().min(Duration::from_secs(2));
    let mut bytes = Vec::new();
    let end = loop {
        if cancelled() || Instant::now() >= deadline {
            bail!("Browser callback interrupted");
        }
        let mut buffer = [0u8; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => bail!("Browser callback closed before headers"),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > CALLBACK_BYTES {
                    bail!("Browser callback exceeds its byte bound");
                }
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => bail!("Browser callback could not be read"),
        }
    };
    let headers = std::str::from_utf8(&bytes[..end])?;
    let mut lines = headers.split("\r\n");
    let request = lines
        .next()
        .context("Browser callback is missing its request")?
        .split(' ')
        .collect::<Vec<_>>();
    if request.len() != 3
        || request[0] != "GET"
        || !matches!(request[2], "HTTP/1.0" | "HTTP/1.1")
        || !request[1].starts_with('/')
        || request[1].starts_with("//")
        || !request[1].bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("Browser callback request is invalid");
    }
    let expected_host = format!(
        "127.0.0.1:{}",
        login
            .redirect_uri
            .port()
            .context("Browser callback has no port")?
    );
    let mut host = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .context("Browser callback header is invalid")?;
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("Browser callback header name is invalid");
        }
        if name.eq_ignore_ascii_case("host") {
            if host || value.trim() != expected_host {
                bail!("Browser callback host is invalid");
            }
            host = true;
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length") && value.trim() != "0"
        {
            bail!("Browser callback cannot contain a body");
        }
    }
    if !host || bytes.len() != end + 4 {
        bail!("Browser callback must contain exactly one header-only request");
    }
    let url = login.redirect_uri.join(request[1])?;
    if url.origin() != login.redirect_uri.origin()
        || url.path() != login.redirect_uri.path()
        || url.fragment().is_some()
    {
        bail!("Browser callback route is invalid");
    }
    let mut fields = HashMap::new();
    for (name, value) in url.query_pairs() {
        if fields
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            bail!("Browser callback contains duplicate parameters");
        }
    }
    if !fields
        .get("state")
        .is_some_and(|state| equal_secret(&login.state, state))
    {
        bail!("Browser callback state does not match this attempt");
    }
    if fields.get("iss").is_some_and(|issuer| {
        Url::parse(&login.authorization_url)
            .map_or(true, |url| issuer != &url.origin().ascii_serialization())
    }) {
        bail!("Browser callback issuer does not match this attempt");
    }
    let code = fields.get("code");
    let error = fields.get("error");
    if code.is_some() == error.is_some()
        || code.or(error).is_none_or(|value| {
            value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        bail!("Browser callback must contain either a code or a denial");
    }
    Ok(fields)
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Aegis\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
}

fn validate_identity(
    tokens: &Value,
    login: &BrowserLogin,
    issuer: &str,
    client: &str,
    code: &str,
) -> Result<()> {
    let identity = opaque(tokens, "id_token")?;
    let claims =
        jwt_claims(&identity).map_err(|_| anyhow!("Browser sign-in identity format is invalid"))?;
    let header = URL_SAFE_NO_PAD
        .decode(identity.split('.').next().unwrap_or(""))
        .map_err(|_| anyhow!("Browser sign-in identity header is invalid"))?;
    let header: Value = serde_json::from_slice(&header)
        .map_err(|_| anyhow!("Browser sign-in identity header is invalid"))?;
    let algorithm = header["alg"]
        .as_str()
        .context("Browser sign-in identity has no algorithm")?;
    if !matches!(
        algorithm,
        "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" | "ES256" | "ES384" | "EdDSA"
    ) || header.get("crit").is_some()
    {
        bail!("Browser sign-in identity uses an unsupported algorithm");
    }
    let audience = claims["aud"]
        .as_str()
        .map(|value| vec![value])
        .or_else(|| {
            claims["aud"]
                .as_array()
                .and_then(|values| values.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        })
        .context("Browser sign-in identity has an invalid audience")?;
    let now = now()?;
    if claims["iss"] != issuer
        || !audience.contains(&client)
        || audience.iter().any(|audience| *audience != client)
        || audience.len() > 1 && claims["azp"] != client
        || claims
            .get("azp")
            .is_some_and(|authorized| authorized != client)
        || claims["sub"]
            .as_str()
            .is_none_or(|subject| subject.is_empty() || subject.len() > 255 || !subject.is_ascii())
        || claims["nonce"]
            .as_str()
            .is_none_or(|nonce| !equal_secret(&login.nonce, nonce))
        || claims["exp"]
            .as_u64()
            .is_none_or(|expiry| expiry <= now || expiry > now.saturating_add(MAX_EXPIRY))
        || claims["iat"].as_u64().is_none_or(|issued| {
            issued > now.saturating_add(60)
                || issued < now.saturating_sub(LOGIN_WINDOW_SECONDS + 60)
        })
    {
        bail!("Browser sign-in identity does not match the issuer, account or this attempt");
    }
    for (field, value) in [
        ("at_hash", tokens["access_token"].as_str().unwrap_or("")),
        ("c_hash", code),
    ] {
        if let Some(expected) = claims.get(field) {
            let digest = match algorithm {
                "RS256" | "PS256" | "ES256" => Sha256::digest(value.as_bytes()).to_vec(),
                "RS384" | "PS384" | "ES384" => Sha384::digest(value.as_bytes()).to_vec(),
                "RS512" | "PS512" => Sha512::digest(value.as_bytes()).to_vec(),
                _ => bail!("Browser sign-in identity hash algorithm is unsupported"),
            };
            let actual = URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2]);
            if expected
                .as_str()
                .is_none_or(|expected| !equal_secret(expected, &actual))
            {
                bail!("Browser sign-in identity does not bind its authorization receipt");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use super::*;

    struct Service {
        origin: Url,
        tokens: Arc<Mutex<Value>>,
        requests: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
        stopped: Arc<AtomicBool>,
        worker: Option<JoinHandle<Result<()>>>,
    }

    impl Service {
        fn start() -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let origin = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
            let server_origin = origin.clone();
            let tokens = Arc::new(Mutex::new(Value::Null));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let reply_tokens = Arc::clone(&tokens);
            let recorded = Arc::clone(&requests);
            let worker_stop = Arc::clone(&stopped);
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut bytes = Vec::new();
                    let (headers, body) = loop {
                        let mut buffer = [0; 4096];
                        let count = stream.read(&mut buffer)?;
                        anyhow::ensure!(
                            count != 0 && bytes.len() + count < MAX_BYTES,
                            "Invalid fixture request"
                        );
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = std::str::from_utf8(&bytes[..end])?;
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break (
                                    headers.to_owned(),
                                    String::from_utf8(bytes[end + 4..end + 4 + length].to_vec())?,
                                );
                            }
                        }
                    };
                    assert!(!headers.to_lowercase().contains("authorization:"));
                    assert!(
                        headers
                            .to_lowercase()
                            .contains(concat!("user-agent: aegis/", env!("CARGO_PKG_VERSION")))
                    );
                    let fields: HashMap<_, _> = url_fields(&body);
                    recorded.lock().unwrap().push((headers.clone(), fields));
                    let response = if headers.starts_with("GET /.well-known/openid-configuration ") {
                        json!({"issuer":server_origin.as_str().trim_end_matches('/'),
                            "authorization_endpoint":server_origin.join("oauth2/authorize")?.to_string(),
                            "token_endpoint":server_origin.join("oauth2/token")?.to_string()})
                    } else {
                        reply_tokens.lock().unwrap().clone()
                    }.to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )?;
                }
                Ok(())
            });
            Ok(Self {
                origin,
                tokens,
                requests,
                stopped,
                worker: Some(worker),
            })
        }

        fn finish(mut self) -> Result<()> {
            self.stopped.store(true, Ordering::Release);
            self.worker
                .take()
                .unwrap()
                .join()
                .map_err(|_| anyhow!("Browser auth fixture panicked"))?
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn url_fields(value: &str) -> HashMap<String, String> {
        let mut url = Url::parse("http://127.0.0.1/").unwrap();
        url.set_query(Some(value));
        url.query_pairs().into_owned().collect()
    }

    fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.fixture-signature",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn tokens(client: &AuthClient, login: &BrowserLogin) -> Result<Value> {
        let now = now()?;
        let access = match client.provider {
            Provider::ChatGpt => jwt(
                json!({"exp":now+3600,"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}),
            ),
            Provider::Grok => "fixture-access".into(),
        };
        let claims = json!({"iss":client.origin.as_str().trim_end_matches('/'),"aud":client.client_id(),
            "sub":"fixture-user","nonce":login.nonce,"exp":now+3600,"iat":now,
            "at_hash":URL_SAFE_NO_PAD.encode(&Sha256::digest(access.as_bytes())[..16]),
            "c_hash":URL_SAFE_NO_PAD.encode(&Sha256::digest(b"fixture-code")[..16]),
            "https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}});
        Ok(
            json!({"access_token":access,"refresh_token":"fixture-refresh","token_type":"Bearer","expires_in":3600,"id_token":jwt(claims)}),
        )
    }

    fn callback(login: &BrowserLogin, query: &str, host: Option<&str>) -> Result<TcpStream> {
        let mut stream = TcpStream::connect(("127.0.0.1", login.redirect_uri.port().unwrap()))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        write!(
            stream,
            "GET {}?{query} HTTP/1.1\r\nHost: {}\r\n\r\n",
            login.redirect_uri.path(),
            host.map(str::to_owned)
                .unwrap_or_else(|| format!("127.0.0.1:{}", login.redirect_uri.port().unwrap()))
        )?;
        Ok(stream)
    }

    #[test]
    fn both_browser_flows_bind_pkce_state_nonce_and_single_use_exchange() -> Result<()> {
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let service = Service::start()?;
            let client = AuthClient {
                provider,
                origin: service.origin.clone(),
            };
            let mut login = client.begin_browser_on(&[0], &|| false)?;
            let url = Url::parse(login.authorization_url())?;
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(query["response_type"], "code");
            assert_eq!(query["code_challenge_method"], "S256");
            assert_eq!(query["client_id"], client.client_id());
            assert_eq!(query["redirect_uri"], login.redirect_uri.as_str());
            assert_eq!(query["state"].len(), 43);
            assert_eq!(query["nonce"].len(), 43);
            assert_ne!(query["state"], query["nonce"]);
            assert_ne!(login.verifier, query["state"]);
            assert!(!login.authorization_url().contains(&login.verifier));
            assert!(!query["scope"].contains("connectors"));
            assert!(!query["scope"].contains("conversations"));
            *service.tokens.lock().unwrap() = tokens(&client, &login)?;
            let mut stream = callback(
                &login,
                &format!("code=fixture-code&state={}", login.state),
                None,
            )?;
            let session = match client.poll_browser(&mut login, || false)? {
                Poll::SignedIn(session) => session,
                _ => bail!("Fixture callback was not accepted"),
            };
            let mut page = String::new();
            stream.read_to_string(&mut page)?;
            assert!(page.contains("verifying sign-in"));
            assert!(!page.contains("fixture-code"));
            assert!(!page.contains(&login.state));
            assert!(!page.contains("fixture-refresh"));
            assert!(login.listener.is_none());
            assert_eq!(session.provider, client.name());
            let requests = service.requests.lock().unwrap().clone();
            let exchanges = requests
                .iter()
                .filter(|(headers, _)| headers.starts_with("POST "))
                .collect::<Vec<_>>();
            assert_eq!(exchanges.len(), 1);
            let fields = &exchanges[0].1;
            assert_eq!(fields["code"], "fixture-code");
            assert_eq!(fields["redirect_uri"], query["redirect_uri"]);
            assert_eq!(fields["grant_type"], "authorization_code");
            assert_eq!(
                URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
                query["code_challenge"]
            );
            assert!(client.poll_browser(&mut login, || false).is_err());
            assert_eq!(service.requests.lock().unwrap().len(), requests.len());
            service.finish()?;
        }
        Ok(())
    }

    #[test]
    fn wrong_missing_duplicate_and_cross_host_callbacks_cannot_spend_the_code() -> Result<()> {
        let service = Service::start()?;
        let client = AuthClient {
            provider: Provider::ChatGpt,
            origin: service.origin.clone(),
        };
        let mut login = client.begin_browser_on(&[0], &|| false)?;
        for (query, host) in [
            ("code=fixture-code".into(), None),
            ("code=fixture-code&state=wrong".into(), None),
            (
                format!(
                    "code=fixture-code&state={}&state={}",
                    login.state, login.state
                ),
                None,
            ),
            (
                format!("code=fixture-code&state={}", login.state),
                Some("attacker.example"),
            ),
            (
                format!("code=fixture-code&error=denied&state={}", login.state),
                None,
            ),
            (
                format!(
                    "code=fixture-code&iss=https%3A%2F%2Fattacker.example&state={}",
                    login.state
                ),
                None,
            ),
        ] {
            let mut stream = callback(&login, &query, host)?;
            assert!(matches!(
                client.poll_browser(&mut login, || false)?,
                Poll::Pending(_)
            ));
            let mut page = String::new();
            stream.read_to_string(&mut page)?;
            assert!(page.starts_with("HTTP/1.1 400"));
            assert!(!page.contains("fixture-code"));
            assert!(!login.finished);
            assert!(service.requests.lock().unwrap().is_empty());
        }
        let port = login.redirect_uri.port().unwrap();
        assert!(client.poll_browser(&mut login, || true).is_err());
        assert!(login.listener.is_none());
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
        service.finish()?;
        Ok(())
    }

    #[test]
    fn denied_expired_and_invalid_identity_attempts_are_terminal_without_replay() -> Result<()> {
        let service = Service::start()?;
        let client = AuthClient {
            provider: Provider::ChatGpt,
            origin: service.origin.clone(),
        };
        let mut denied = client.begin_browser_on(&[0], &|| false)?;
        let _stream = callback(
            &denied,
            &format!(
                "error=access_denied&error_description=fixture-private&state={}",
                denied.state
            ),
            None,
        )?;
        let error = client
            .poll_browser(&mut denied, || false)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("fixture-private"));
        assert!(denied.finished);
        assert!(service.requests.lock().unwrap().is_empty());
        let mut expired = client.begin_browser_on(&[0], &|| false)?;
        expired.deadline = Instant::now();
        assert!(client.poll_browser(&mut expired, || false).is_err());
        assert!(expired.listener.is_none());
        let mut invalid = client.begin_browser_on(&[0], &|| false)?;
        *service.tokens.lock().unwrap() =
            json!({"access_token":"fixture-private-access","id_token":"invalid"});
        let _stream = callback(
            &invalid,
            &format!("code=fixture-code&state={}", invalid.state),
            None,
        )?;
        let error = client
            .poll_browser(&mut invalid, || false)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("fixture-private-access"));
        assert!(invalid.finished);
        assert!(client.poll_browser(&mut invalid, || false).is_err());
        assert_eq!(service.requests.lock().unwrap().len(), 1);
        service.finish()?;
        Ok(())
    }

    #[test]
    fn identity_claims_and_discovery_are_bound_to_the_trusted_exchange() -> Result<()> {
        let service = Service::start()?;
        let client = AuthClient {
            provider: Provider::ChatGpt,
            origin: service.origin.clone(),
        };
        let login = client.begin_browser_on(&[0], &|| false)?;
        let valid = tokens(&client, &login)?;
        validate_identity(
            &valid,
            &login,
            client.origin.as_str().trim_end_matches('/'),
            client.client_id(),
            "fixture-code",
        )?;
        for (field, value) in [
            ("iss", json!("https://attacker.example")),
            ("aud", json!("other-client")),
            ("azp", json!("other-client")),
            ("nonce", json!("other-nonce")),
            ("sub", json!("")),
            ("iat", json!(0)),
            ("exp", json!(0)),
            ("at_hash", json!("wrong")),
            ("c_hash", json!("wrong")),
            ("aud", json!([client.client_id(), "other-client"])),
        ] {
            let mut altered = valid.clone();
            let mut claims = jwt_claims(valid["id_token"].as_str().unwrap())?;
            claims[field] = value;
            altered["id_token"] = json!(jwt(claims));
            assert!(
                validate_identity(
                    &altered,
                    &login,
                    client.origin.as_str().trim_end_matches('/'),
                    client.client_id(),
                    "fixture-code"
                )
                .is_err(),
                "{field}"
            );
        }
        for endpoint in [
            "https://attacker.example/token",
            "http://user@127.0.0.1/token",
            "http://127.0.0.1/token?secret=1",
            "http://127.0.0.1/token#fragment",
        ] {
            assert!(
                trusted_endpoint(
                    &client.origin,
                    &json!({"token_endpoint":endpoint}),
                    "token_endpoint"
                )
                .is_err()
            );
        }
        let mut claims = jwt_claims(valid["id_token"].as_str().unwrap())?;
        claims["aud"] = json!([client.client_id(), "other-client"]);
        claims["azp"] = json!(client.client_id());
        let mut altered = valid.clone();
        altered["id_token"] = json!(jwt(claims));
        assert!(
            validate_identity(
                &altered,
                &login,
                client.origin.as_str().trim_end_matches('/'),
                client.client_id(),
                "fixture-code"
            )
            .is_err()
        );
        let mut unsigned = valid.clone();
        let fields = valid["id_token"]
            .as_str()
            .unwrap()
            .split('.')
            .collect::<Vec<_>>();
        unsigned["id_token"] = json!(format!(
            "{}.{}.fixture-signature",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            fields[1]
        ));
        assert!(
            validate_identity(
                &unsigned,
                &login,
                client.origin.as_str().trim_end_matches('/'),
                client.client_id(),
                "fixture-code"
            )
            .is_err()
        );
        service.finish()?;
        Ok(())
    }

    #[test]
    fn callback_bounds_cancellation_and_port_conflicts_release_owned_listeners() -> Result<()> {
        let client = AuthClient::new(Provider::ChatGpt)?;
        let occupied = TcpListener::bind("127.0.0.1:0")?;
        let occupied_port = occupied.local_addr()?.port();
        assert!(
            client
                .begin_browser_on(&[occupied_port], &|| false)
                .is_err()
        );
        let mut login = client.begin_browser_on(&[occupied_port, 0], &|| false)?;
        assert_ne!(login.redirect_uri.port(), Some(occupied_port));
        let port = login.redirect_uri.port().unwrap();
        for target in [
            "POST /auth/callback HTTP/1.1",
            "GET /not-the-callback HTTP/1.1",
            "GET http://attacker.example/auth/callback HTTP/1.1",
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", port))?;
            write!(stream, "{target}\r\nHost: 127.0.0.1:{port}\r\n\r\n")?;
            assert!(matches!(
                client.poll_browser(&mut login, || false)?,
                Poll::Pending(_)
            ));
            assert!(!login.finished);
        }
        let mut oversized = TcpStream::connect(("127.0.0.1", port))?;
        oversized.write_all(&vec![b'x'; CALLBACK_BYTES + 1])?;
        assert!(matches!(
            client.poll_browser(&mut login, || false)?,
            Poll::Pending(_)
        ));
        assert!(!login.finished);
        let _slow = TcpStream::connect(("127.0.0.1", port))?;
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let started = Instant::now();
        assert!(
            client
                .poll_browser(&mut login, || checks.fetch_add(1, Ordering::SeqCst) > 3)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(login.finished && login.listener.is_none());
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
        assert!(client.begin_browser_on(&[0], &|| true).is_err());
        Ok(())
    }
}
