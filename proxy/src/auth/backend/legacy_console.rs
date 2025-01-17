//! Cloud API V1.

use super::DatabaseInfo;
use crate::{
    auth::{self, ClientCredentials},
    compute,
    error::UserFacingError,
    stream::PqStream,
    waiters,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use utils::pq_proto::BeMessage as Be;

#[derive(Debug, Error)]
pub enum LegacyAuthError {
    /// Authentication error reported by the console.
    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    /// HTTP status (other than 200) returned by the console.
    #[error("Console responded with an HTTP status: {0}")]
    HttpStatus(reqwest::StatusCode),

    #[error("Console responded with a malformed JSON: {0}")]
    BadResponse(#[from] serde_json::Error),

    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    #[error(transparent)]
    WaiterRegister(#[from] waiters::RegisterError),

    #[error(transparent)]
    WaiterWait(#[from] waiters::WaitError),
}

impl UserFacingError for LegacyAuthError {
    fn to_string_client(&self) -> String {
        use LegacyAuthError::*;
        match self {
            AuthFailed(_) | HttpStatus(_) => self.to_string(),
            _ => "Internal error".to_string(),
        }
    }
}

// NOTE: the order of constructors is important.
// https://serde.rs/enum-representations.html#untagged
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum ProxyAuthResponse {
    Ready { conn_info: DatabaseInfo },
    Error { error: String },
    NotReady { ready: bool }, // TODO: get rid of `ready`
}

impl ClientCredentials {
    fn is_existing_user(&self) -> bool {
        self.user.ends_with("@zenith")
    }
}

async fn authenticate_proxy_client(
    auth_endpoint: &reqwest::Url,
    creds: &ClientCredentials,
    md5_response: &str,
    salt: &[u8; 4],
    psql_session_id: &str,
) -> Result<DatabaseInfo, LegacyAuthError> {
    let mut url = auth_endpoint.clone();
    url.query_pairs_mut()
        .append_pair("login", &creds.user)
        .append_pair("database", &creds.dbname)
        .append_pair("md5response", md5_response)
        .append_pair("salt", &hex::encode(salt))
        .append_pair("psql_session_id", psql_session_id);

    super::with_waiter(psql_session_id, |waiter| async {
        println!("cloud request: {}", url);
        // TODO: leverage `reqwest::Client` to reuse connections
        let resp = reqwest::get(url).await?;
        if !resp.status().is_success() {
            return Err(LegacyAuthError::HttpStatus(resp.status()));
        }

        let auth_info = serde_json::from_str(resp.text().await?.as_str())?;
        println!("got auth info: {:?}", auth_info);

        use ProxyAuthResponse::*;
        let db_info = match auth_info {
            Ready { conn_info } => conn_info,
            Error { error } => return Err(LegacyAuthError::AuthFailed(error)),
            NotReady { .. } => waiter.await?.map_err(LegacyAuthError::AuthFailed)?,
        };

        Ok(db_info)
    })
    .await
}

async fn handle_existing_user(
    auth_endpoint: &reqwest::Url,
    client: &mut PqStream<impl AsyncRead + AsyncWrite + Unpin + Send>,
    creds: &ClientCredentials,
) -> auth::Result<compute::NodeInfo> {
    let psql_session_id = super::link::new_psql_session_id();
    let md5_salt = rand::random();

    client
        .write_message(&Be::AuthenticationMD5Password(md5_salt))
        .await?;

    // Read client's password hash
    let msg = client.read_password_message().await?;
    let md5_response = parse_password(&msg).ok_or(auth::AuthErrorImpl::MalformedPassword(
        "the password should be a valid null-terminated utf-8 string",
    ))?;

    let db_info = authenticate_proxy_client(
        auth_endpoint,
        creds,
        md5_response,
        &md5_salt,
        &psql_session_id,
    )
    .await?;

    Ok(compute::NodeInfo {
        reported_auth_ok: false,
        config: db_info.into(),
    })
}

pub async fn handle_user(
    auth_endpoint: &reqwest::Url,
    auth_link_uri: &reqwest::Url,
    creds: &ClientCredentials,
    client: &mut PqStream<impl AsyncRead + AsyncWrite + Unpin + Send>,
) -> auth::Result<compute::NodeInfo> {
    if creds.is_existing_user() {
        handle_existing_user(auth_endpoint, client, creds).await
    } else {
        super::link::handle_user(auth_link_uri, client).await
    }
}

fn parse_password(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()?.strip_suffix('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_proxy_auth_response() {
        // Ready
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": true,
            "conn_info": DatabaseInfo::default(),
        }))
        .unwrap();
        assert!(matches!(
            auth,
            ProxyAuthResponse::Ready {
                conn_info: DatabaseInfo { .. }
            }
        ));

        // Error
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": false,
            "error": "too bad, so sad",
        }))
        .unwrap();
        assert!(matches!(auth, ProxyAuthResponse::Error { .. }));

        // NotReady
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": false,
        }))
        .unwrap();
        assert!(matches!(auth, ProxyAuthResponse::NotReady { .. }));
    }

    #[test]
    fn parse_db_info() -> anyhow::Result<()> {
        let _: DatabaseInfo = serde_json::from_value(json!({
            "host": "localhost",
            "port": 5432,
            "dbname": "postgres",
            "user": "john_doe",
            "password": "password",
        }))?;

        let _: DatabaseInfo = serde_json::from_value(json!({
            "host": "localhost",
            "port": 5432,
            "dbname": "postgres",
            "user": "john_doe",
        }))?;

        Ok(())
    }
}
