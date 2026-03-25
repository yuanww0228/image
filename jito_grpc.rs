use anyhow::{Context, Result};
use prost_types::Timestamp;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::VersionedTransaction;
use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tonic::codegen::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Status, metadata::MetadataValue};

use crate::jito::{auth, bundle, packet, searcher, shared};
use once_cell::sync::Lazy;

static SEARCHER_CLIENTS: Lazy<RwLock<HashMap<String, SearcherClientHolder>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// 全局 token 缓存：读多写少的场景用 RwLock，避免频繁重新鉴权
static TOKEN_CACHE: Lazy<RwLock<HashMap<String, TokenState>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// 缓存 access/refresh 及其过期时间
struct TokenState {
    access_token: String,
    access_expires_at: Option<Timestamp>,
    refresh_token: String,
    refresh_expires_at: Option<Timestamp>,
}

struct SearcherClientHolder {
    client: searcher::searcher_service_client::SearcherServiceClient<
        InterceptedService<Channel, ClientInterceptor>,
    >,
}

/// Fetch a short-lived access token from Jito's AuthService (role=SEARCHER).
pub async fn fetch_access_token(auth_url: &str, keypair: &Keypair) -> Result<String> {
    let state = fetch_tokens(auth_url, keypair).await?;
    Ok(state.access_token)
}

/// 返回可复用的 access_token：优先读缓存，快过期时走 refresh，否则重新鉴权
pub async fn get_cached_access_token(auth_url: &str, keypair: &Keypair) -> Result<String> {
    let cache_key = token_cache_key(auth_url, keypair);

    // 快路径：读锁读取有效 token
    if let Some(token) = read_cached_token(&cache_key).await {
        return Ok(token);
    }

    // 慢路径：写锁刷新/重建 token
    let mut guard = TOKEN_CACHE.write().await;
    if let Some(state) = guard.get(&cache_key) {
        if !is_expiring_soon(&state.access_expires_at, Duration::from_secs(60)) {
            return Ok(state.access_token.clone());
        }
    }

    if let Some(state) = guard.get_mut(&cache_key) {
        if !is_expiring_soon(&state.refresh_expires_at, Duration::from_secs(60)) {
            if let Ok((access_token, access_expires_at)) =
                refresh_access_token(auth_url, &state.refresh_token).await
            {
                state.access_token = access_token.clone();
                state.access_expires_at = access_expires_at;
                return Ok(access_token);
            }
        }
    }

    let new_state = fetch_tokens(auth_url, keypair).await?;
    let token = new_state.access_token.clone();
    guard.insert(cache_key, new_state);
    Ok(token)
}

/// Build a bundle from already-serialized transactions.
pub fn build_bundle_from_packets(txs: Vec<Vec<u8>>) -> Result<bundle::Bundle> {
    let header = shared::Header {
        ts: Some(now_timestamp()?),
    };

    let packets = txs
        .into_iter()
        .map(|data| packet::Packet {
            meta: Some(packet::Meta {
                size: data.len() as u64,
                ..Default::default()
            }),
            data,
        })
        .collect();

    Ok(bundle::Bundle {
        header: Some(header),
        packets,
    })
}

/// Convenience: serialize VersionedTransaction with bincode.
pub fn serialize_versioned_tx(tx: &VersionedTransaction) -> Result<Vec<u8>> {
    bincode::serialize(tx).context("bincode serialize versioned transaction")
}

/// Send a bundle using a fresh access token from AuthService.
pub async fn send_bundle_grpc(
    auth_url: &str,
    searcher_url: &str,
    keypair: &Keypair,
    txs: Vec<Vec<u8>>,
) -> Result<String> {
    send_bundle_grpc_auto(auth_url, searcher_url, keypair, txs).await
}

/// 使用后台自动刷新的 token 发送 bundle（推荐）
pub async fn send_bundle_grpc_auto(
    auth_url: &str,
    searcher_url: &str,
    keypair: &Keypair,
    txs: Vec<Vec<u8>>,
) -> Result<String> {
    let mut client = get_searcher_client_auth_cached(auth_url, searcher_url, keypair).await?;
    let bundle = build_bundle_from_packets(txs)?;
    let response = client
        .send_bundle(searcher::SendBundleRequest {
            bundle: Some(bundle),
        })
        .await
        .context("send_bundle")?
        .into_inner();
    Ok(response.uuid)
}

/// Send a bundle using a pre-fetched access token.
pub async fn send_bundle_with_token(
    searcher_url: &str,
    access_token: &str,
    txs: Vec<Vec<u8>>,
) -> Result<String> {
    let channel = create_grpc_channel(searcher_url).await?;
    let mut client = searcher::searcher_service_client::SearcherServiceClient::new(channel);

    let bundle = build_bundle_from_packets(txs)?;
    let mut request = Request::new(searcher::SendBundleRequest {
        bundle: Some(bundle),
    });

    let auth_value = MetadataValue::try_from(format!("Bearer {}", access_token))
        .context("build authorization metadata")?;
    request.metadata_mut().insert("authorization", auth_value);

    let response = client
        .send_bundle(request)
        .await
        .context("send_bundle")?
        .into_inner();
    Ok(response.uuid)
}

fn now_timestamp() -> Result<Timestamp> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before unix epoch")?;
    Ok(Timestamp {
        seconds: now.as_secs() as i64,
        nanos: now.subsec_nanos() as i32,
    })
}

// 创建 gRPC 连接，https 自动启用 TLS + 根证书
async fn create_grpc_channel(url: &str) -> Result<Channel> {
    let mut endpoint = Endpoint::from_shared(url.to_string()).context("invalid grpc url")?;
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_enabled_roots())?;
    }
    Ok(endpoint.connect().await?)
}

async fn get_searcher_client_auth_cached(
    auth_url: &str,
    searcher_url: &str,
    keypair: &Keypair,
) -> Result<
    searcher::searcher_service_client::SearcherServiceClient<
        InterceptedService<Channel, ClientInterceptor>,
    >,
> {
    let cache_key = format!("{}|{}|{}", auth_url, searcher_url, keypair.pubkey());

    {
        let guard = SEARCHER_CLIENTS.read().await;
        if let Some(holder) = guard.get(&cache_key) {
            return Ok(holder.client.clone());
        }
    }

    let auth_channel = create_grpc_channel(auth_url).await?;

    let mut secret = [0u8; 32];
    let key_bytes = keypair.to_bytes();
    secret.copy_from_slice(&key_bytes[..32]);
    let auth_keypair = Arc::new(Keypair::new_from_array(secret));

    let interceptor = ClientInterceptor::new(
        auth::auth_service_client::AuthServiceClient::new(auth_channel),
        auth_keypair,
        auth::Role::Searcher,
    )
    .await?;

    let searcher_channel = create_grpc_channel(searcher_url).await?;
    let client = searcher::searcher_service_client::SearcherServiceClient::with_interceptor(
        searcher_channel,
        interceptor,
    );

    let mut guard = SEARCHER_CLIENTS.write().await;
    if let Some(holder) = guard.get(&cache_key) {
        return Ok(holder.client.clone());
    }
    guard.insert(
        cache_key,
        SearcherClientHolder {
            client: client.clone(),
        },
    );

    Ok(client)
}

// 仅在 access_token 仍未接近过期时复用
fn token_cache_key(auth_url: &str, keypair: &Keypair) -> String {
    format!("{}|{}", auth_url, keypair.pubkey())
}

async fn read_cached_token(cache_key: &str) -> Option<String> {
    let guard = TOKEN_CACHE.read().await;
    match guard.get(cache_key) {
        Some(state) if !is_expiring_soon(&state.access_expires_at, Duration::from_secs(60)) => {
            Some(state.access_token.clone())
        }
        _ => None,
    }
}

// 完整鉴权流程：challenge + 签名 + 交换 token
async fn fetch_tokens(auth_url: &str, keypair: &Keypair) -> Result<TokenState> {
    let channel = create_grpc_channel(auth_url).await?;
    let mut client = auth::auth_service_client::AuthServiceClient::new(channel);

    let challenge_resp = client
        .generate_auth_challenge(auth::GenerateAuthChallengeRequest {
            role: auth::Role::Searcher as i32,
            pubkey: keypair.pubkey().to_bytes().to_vec(),
        })
        .await
        .context("generate_auth_challenge")?
        .into_inner();

    let challenge = format!("{}-{}", keypair.pubkey(), challenge_resp.challenge);
    let signature = keypair.sign_message(challenge.as_bytes());

    let token_resp = client
        .generate_auth_tokens(auth::GenerateAuthTokensRequest {
            challenge,
            client_pubkey: keypair.pubkey().to_bytes().to_vec(),
            signed_challenge: signature.as_ref().to_vec(),
        })
        .await
        .context("generate_auth_tokens")?
        .into_inner();

    let access = token_resp
        .access_token
        .context("missing access_token in response")?;
    let refresh = token_resp
        .refresh_token
        .context("missing refresh_token in response")?;

    Ok(TokenState {
        access_token: access.value,
        access_expires_at: access.expires_at_utc,
        refresh_token: refresh.value,
        refresh_expires_at: refresh.expires_at_utc,
    })
}

// 使用 refresh_token 换新的 access_token（速度更快）
async fn refresh_access_token(
    auth_url: &str,
    refresh_token: &str,
) -> Result<(String, Option<Timestamp>)> {
    let channel = create_grpc_channel(auth_url).await?;
    let mut client = auth::auth_service_client::AuthServiceClient::new(channel);
    let resp = client
        .refresh_access_token(auth::RefreshAccessTokenRequest {
            refresh_token: refresh_token.to_string(),
        })
        .await
        .context("refresh_access_token")?
        .into_inner();

    let access = resp
        .access_token
        .context("missing access_token in refresh response")?;
    Ok((access.value, access.expires_at_utc))
}

// 判断 token 是否即将过期（margin 作为提前刷新窗口）
fn is_expiring_soon(ts: &Option<Timestamp>, margin: Duration) -> bool {
    let Some(ts) = ts else {
        return true;
    };
    let Ok(expiry) = timestamp_to_system_time(ts) else {
        return true;
    };
    let now = SystemTime::now();
    let limit = now.checked_add(margin).unwrap_or(now);
    expiry <= limit
}

// prost Timestamp -> SystemTime
fn timestamp_to_system_time(ts: &Timestamp) -> Result<SystemTime> {
    if ts.seconds < 0 {
        return Err(anyhow::anyhow!("negative timestamp"));
    }
    let dur = Duration::new(ts.seconds as u64, ts.nanos as u32);
    Ok(UNIX_EPOCH + dur)
}

#[derive(Clone)]
struct ClientInterceptor {
    bearer_token: Arc<StdRwLock<String>>,
}

impl ClientInterceptor {
    pub async fn new(
        mut auth_service_client: auth::auth_service_client::AuthServiceClient<Channel>,
        keypair: Arc<Keypair>,
        role: auth::Role,
    ) -> Result<Self> {
        let (access_token, refresh_token) =
            Self::auth(&mut auth_service_client, &keypair, role).await?;

        let bearer_token = Arc::new(StdRwLock::new(access_token.value.clone()));

        Self::spawn_token_refresh_task(
            auth_service_client,
            bearer_token.clone(),
            refresh_token,
            access_token.expires_at_utc,
            keypair,
            role,
        );

        Ok(Self { bearer_token })
    }

    async fn auth(
        auth_service_client: &mut auth::auth_service_client::AuthServiceClient<Channel>,
        keypair: &Keypair,
        role: auth::Role,
    ) -> Result<(auth::Token, auth::Token)> {
        let challenge_resp = auth_service_client
            .generate_auth_challenge(auth::GenerateAuthChallengeRequest {
                role: role as i32,
                pubkey: keypair.pubkey().to_bytes().to_vec(),
            })
            .await
            .context("generate_auth_challenge")?
            .into_inner();

        let challenge = format!("{}-{}", keypair.pubkey(), challenge_resp.challenge);
        let signed_challenge = keypair.sign_message(challenge.as_bytes()).as_ref().to_vec();

        let tokens = auth_service_client
            .generate_auth_tokens(auth::GenerateAuthTokensRequest {
                challenge,
                client_pubkey: keypair.pubkey().to_bytes().to_vec(),
                signed_challenge,
            })
            .await
            .context("generate_auth_tokens")?
            .into_inner();

        let access = tokens
            .access_token
            .context("missing access_token in response")?;
        let refresh = tokens
            .refresh_token
            .context("missing refresh_token in response")?;
        Ok((access, refresh))
    }

    fn spawn_token_refresh_task(
        mut auth_service_client: auth::auth_service_client::AuthServiceClient<Channel>,
        bearer_token: Arc<StdRwLock<String>>,
        refresh_token: auth::Token,
        access_token_expiration: Option<Timestamp>,
        keypair: Arc<Keypair>,
        role: auth::Role,
    ) {
        tokio::spawn(async move {
            let mut refresh_token = refresh_token;
            let mut access_token_expiration = access_token_expiration;

            loop {
                let access_ttl = ttl_until(&access_token_expiration);
                let refresh_ttl = ttl_until(&refresh_token.expires_at_utc);

                let refresh_expiring = refresh_ttl < Duration::from_secs(5 * 60);
                let access_expiring = access_ttl < Duration::from_secs(5 * 60);

                if refresh_expiring {
                    if let Ok((new_access, new_refresh)) =
                        Self::auth(&mut auth_service_client, &keypair, role).await
                    {
                        *bearer_token.write().unwrap() = new_access.value.clone();
                        access_token_expiration = new_access.expires_at_utc;
                        refresh_token = new_refresh;
                    }
                } else if access_expiring {
                    if let Ok(resp) = auth_service_client
                        .refresh_access_token(auth::RefreshAccessTokenRequest {
                            refresh_token: refresh_token.value.clone(),
                        })
                        .await
                    {
                        if let Some(access) = resp.into_inner().access_token {
                            *bearer_token.write().unwrap() = access.value.clone();
                            access_token_expiration = access.expires_at_utc;
                        }
                    }
                } else {
                    sleep(Duration::from_secs(60)).await;
                }
            }
        });
    }
}

impl Interceptor for ClientInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let token = self.bearer_token.read().unwrap();
        if !token.is_empty() {
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {}", *token).parse().unwrap(),
            );
        }
        Ok(request)
    }
}

fn ttl_until(ts: &Option<Timestamp>) -> Duration {
    let Some(ts) = ts else {
        return Duration::from_secs(0);
    };
    let Ok(expiry) = timestamp_to_system_time(ts) else {
        return Duration::from_secs(0);
    };
    expiry
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::from_secs(0))
}
