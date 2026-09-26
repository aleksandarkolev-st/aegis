use std::fs::File;
use std::io::Read;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::storage::{Operation, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkScopes {
    pub domains: Vec<String>,
    pub body_bytes: u64,
}

impl NetworkScopes {
    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?.take(8193).read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            bail!("network scopes file exceeds 8192 bytes");
        }
        let scopes: Self = serde_json::from_slice(&bytes).context("invalid network scopes file")?;
        scopes.validate()?;
        Ok(scopes)
    }
    pub fn validate(&self) -> Result<()> {
        if self.domains.len() > 32 || !(1..=512 * 1024 * 1024).contains(&self.body_bytes) {
            bail!("network scopes require at most 32 domains and 1..536870912 body bytes");
        }
        for domain in &self.domains {
            if domain.len() > 253
                || !domain.is_ascii()
                || domain != &domain.to_ascii_lowercase()
                || domain.split('.').count() < 2
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || !label.as_bytes()[0].is_ascii_alphanumeric()
                        || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
                || domain.parse::<IpAddr>().is_ok()
            {
                bail!(
                    "network scopes require exact lowercase DNS domains, not URLs, IP addresses or wildcards"
                );
            }
        }
        Ok(())
    }

    pub fn from_configuration(configuration: &Value) -> Result<Option<Self>> {
        let Some(value) = configuration
            .get("network_scopes")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let scopes: Self =
            serde_json::from_value(value.clone()).context("invalid network scopes")?;
        scopes.validate()?;
        Ok(Some(scopes))
    }

    pub fn authorize(&self, address: &str) -> Result<Url> {
        self.validate()?;
        if address.len() > 2048 || address.chars().any(char::is_control) {
            bail!("network URL exceeds limits");
        }
        let url = Url::parse(address).context("invalid network URL")?;
        if url.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "api_key"
                    | "apikey"
                    | "access_token"
                    | "token"
                    | "authorization"
                    | "password"
                    | "secret"
                    | "signature"
                    | "credential"
            )
        }) {
            bail!("scoped public web reads cannot carry credential query parameters");
        }
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(443)
            || !url
                .host_str()
                .is_some_and(|domain| self.domains.iter().any(|approved| approved == domain))
        {
            bail!("network URL is outside approved HTTPS domain scopes");
        }
        Ok(url)
    }
}

pub(crate) fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !address.is_private()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_multicast()
                && !address.is_unspecified()
                && !address.is_broadcast()
                && octets[0] != 0
                && octets[0] < 224
                && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
                && !(octets[0] == 192
                    && ((octets[1] == 0 && (octets[2] == 0 || octets[2] == 2))
                        || (octets[1] == 88 && octets[2] == 99)))
                && !(octets[0] == 198
                    && ((octets[1] == 18 || octets[1] == 19)
                        || (octets[1] == 51 && octets[2] == 100)))
                && !(octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            segments[0] & 0xe000 == 0x2000
                && segments[0] != 0x2002
                && segments[0] != 0x3fff
                && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        }
    }
}

fn bounded<Request, Output>(timeout: Duration, request: Request) -> Result<Output>
where
    Request: std::future::Future<Output = Result<Output>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(async { tokio::time::timeout(timeout, request).await })
        .context("scoped network deadline exceeded")?
}

async fn read_body(mut response: reqwest::Response, limit: u64) -> Result<(Vec<u8>, u16)> {
    if !response.status().is_success() {
        bail!(
            "scoped HTTPS returned HTTP {}; response body omitted",
            response.status().as_u16()
        );
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != "identity")
    {
        bail!("scoped HTTPS response must not use compression");
    }
    if response.content_length().is_some_and(|bytes| bytes > limit) {
        bail!("network body exceeds its reserved byte limit");
    }
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("network response interrupted")?
    {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            bail!("network body exceeds its reserved byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, status))
}

pub(crate) fn fetch(store: &mut Store, operation: &Operation) -> Result<Value> {
    let run = store.run(&operation.run_id)?;
    let scopes =
        NetworkScopes::from_configuration(&run.budgets)?.context("network scopes missing")?;
    let url = scopes.authorize(
        operation.arguments["url"]
            .as_str()
            .context("network URL missing")?,
    )?;
    let host = url.host_str().context("network host missing")?.to_owned();
    let (attempt, limit) = store.reserve_network(operation)?;
    let seconds = run.budgets["process_seconds"]
        .as_u64()
        .unwrap_or(60)
        .min(crate::kernel::remaining_seconds(store, &run)?);
    if seconds == 0 {
        bail!("task deadline reached before network request");
    }
    let timeout = Duration::from_secs(seconds);
    let (bytes, status) = bounded(timeout, async {
        let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 443))
            .await
            .context("network DNS lookup failed")?
            .take(33)
            .collect();
        if addresses.is_empty()
            || addresses.len() > 32
            || addresses
                .iter()
                .any(|address| !public_address(address.ip()))
        {
            bail!("network DNS includes a private or special-use address");
        }
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .resolve_to_addrs(&host, &addresses)
            .timeout(timeout)
            .build()?;
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .context("scoped HTTPS request failed")?;
        read_body(response, limit).await
    })?;
    store.finish_network_receipt(operation, attempt, bytes.len() as u64)?;
    let hash = store.put_artifact(&bytes)?;
    store.link_artifact(&operation.id, &hash, "network.body")?;
    Ok(
        json!({"host":host,"status":status,"bytes":bytes.len(),"output_artifact":hash,"attempt":attempt}),
    )
}

impl Store {
    pub fn network_body_charge(&self, run_id: &str) -> Result<u64> {
        Ok(self.connection.query_row("SELECT COALESCE(SUM(CASE WHEN complete=1 THEN received ELSE reserved END),0) FROM network_receipts WHERE run_id=?1",[run_id],|row| row.get(0))?)
    }

    pub(crate) fn reserve_network(&mut self, operation: &Operation) -> Result<(u64, u64)> {
        let run = self.run(&operation.run_id)?;
        let scopes = NetworkScopes::from_configuration(&run.budgets)?
            .context("network access has not been approved")?;
        if operation.capability != "network.fetch"
            || !run
                .grants
                .as_array()
                .is_some_and(|grants| grants.iter().any(|grant| grant == "network.fetch"))
        {
            bail!("network capability is not granted");
        }
        let transaction = self.connection.transaction()?;
        let state: String = transaction.query_row(
            "SELECT state FROM operations WHERE id=?1 AND run_id=?2",
            params![operation.id, operation.run_id],
            |row| row.get(0),
        )?;
        if state != "executing" {
            bail!("network reservation requires a claimed operation");
        }
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id=?1", [&run.id], |row| {
                row.get(0)
            })?;
        if state != "running" {
            bail!("network reservation requires an active task");
        }
        let charged: u64 = transaction.query_row("SELECT COALESCE(SUM(CASE WHEN complete=1 THEN received ELSE reserved END),0) FROM network_receipts WHERE run_id=?1",[&run.id],|row| row.get(0))?;
        let reserved = scopes.body_bytes.saturating_sub(charged).min(1024 * 1024);
        if reserved == 0 {
            bail!("network body byte budget exhausted");
        }
        let attempt: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(attempt),0)+1 FROM network_receipts WHERE operation_id=?1",
            [&operation.id],
            |row| row.get(0),
        )?;
        transaction.execute("INSERT INTO network_receipts(operation_id,attempt,run_id,reserved,received,complete) VALUES (?1,?2,?3,?4,0,0)",params![operation.id,attempt,run.id,reserved])?;
        crate::storage::append_event(
            &transaction,
            &run.id,
            "network.reserved",
            serde_json::json!({"operation":operation.id,"attempt":attempt,"body_bytes":reserved,"metric":"retained HTTP body bytes; not transport/provider traffic"}),
        )?;
        transaction.commit()?;
        Ok((attempt, reserved))
    }

    pub(crate) fn finish_network_receipt(
        &mut self,
        operation: &Operation,
        attempt: u64,
        received: u64,
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute("UPDATE network_receipts SET received=?3,complete=1 WHERE operation_id=?1 AND attempt=?2 AND complete=0 AND reserved>=?3",params![operation.id,attempt,received])?;
        if changed != 1 {
            bail!("invalid or already finalized network receipt");
        }
        crate::storage::append_event(
            &transaction,
            &operation.run_id,
            "network.received",
            serde_json::json!({"operation":operation.id,"attempt":attempt,"body_bytes":received}),
        )?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_reader_enforces_declared_chunked_and_compression_limits_without_following_redirects()
    -> Result<()> {
        use std::io::Write;
        let responses = [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                true,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 128\r\nConnection: close\r\n\r\n",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\n12345678\r\n0\r\n\r\n",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
                false,
            ),
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                false,
            ),
        ];
        for (response, success) in responses {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let server = std::thread::spawn(move || -> std::io::Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut request = [0; 4096];
                stream.read(&mut request)?;
                stream.write_all(response.as_bytes())?;
                Ok(())
            });
            let result = bounded(Duration::from_secs(2), async {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                read_body(client.get(format!("http://{address}/")).send().await?, 8).await
            });
            assert_eq!(result.is_ok(), success);
            if success {
                assert_eq!(result?.0, b"hello");
            }
            server.join().unwrap()?;
        }
        Ok(())
    }

    #[test]
    fn request_deadlines_are_constructed_inside_the_runtime_and_actually_expire() -> Result<()> {
        assert_eq!(bounded(Duration::from_secs(1), async { Ok(7) })?, 7);
        let result: Result<()> = bounded(Duration::from_millis(2), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        });
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn domain_scopes_reject_credentials_aliases_ports_and_private_address_families() -> Result<()> {
        let scopes = NetworkScopes {
            domains: vec!["example.com".into()],
            body_bytes: 2048,
        };
        scopes.authorize("https://example.com/docs")?;
        for url in [
            "http://example.com/",
            "https://sub.example.com/",
            "https://example.com.evil.test/",
            "https://example.com:444/",
            "https://user@example.com/",
            "https://example.com/#fragment",
            "https://127.0.0.1/",
            "https://example.com./",
        ] {
            assert!(scopes.authorize(url).is_err(), "{url}");
        }
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::1",
        ] {
            assert!(!public_address(address.parse()?));
        }
        assert!(public_address("8.8.8.8".parse()?));
        assert!(public_address("2606:4700:4700::1111".parse()?));
        for domain in [
            "*.example.com",
            "Example.com",
            "localhost",
            "127.0.0.1",
            "https://example.com",
            "example..com",
        ] {
            assert!(
                NetworkScopes {
                    domains: vec![domain.into()],
                    body_bytes: 2048
                }
                .validate()
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn interrupted_receipts_charge_their_reservation_and_retries_need_new_budget() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "network",
            directory.path(),
            "fixture",
            json!(["network.fetch"]),
            json!({"network_scopes":{"domains":["example.com"],"body_bytes":2048}}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "network.fetch",
            json!({"url":"https://example.com/"}),
            true,
        )?;
        assert!(store.reserve_network(&operation).is_err());
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.claim_operation(&operation)?;
        let (attempt, reserved) = store.reserve_network(&operation)?;
        assert_eq!(reserved, 2048);
        assert_eq!(store.network_body_charge(&run.id)?, 2048);
        assert!(store.reserve_network(&operation).is_err());
        assert!(
            store
                .finish_network_receipt(&operation, attempt, 2049)
                .is_err()
        );
        store.finish_network_receipt(&operation, attempt, 512)?;
        assert_eq!(store.network_body_charge(&run.id)?, 512);
        assert!(
            store
                .finish_network_receipt(&operation, attempt, 0)
                .is_err()
        );
        let (_, reserved) = store.reserve_network(&operation)?;
        assert_eq!(reserved, 1536);
        drop(store);
        assert_eq!(
            Store::open(directory.path())?.network_body_charge(&run.id)?,
            2048
        );
        Ok(())
    }
}
