//! The one direct-vs-relay dispatch seam for the REST surface.
//!
//! Every `gateway_api` helper is generic over [`GatewayJsonClient`], whose
//! RPITIT methods rule out a `dyn` object — so the two concrete clients
//! (direct's [`DirectHttp`], relay's [`GatewayApi`] tunnel) are unified by
//! this enum instead. `BayboClient::gateway_client` resolves the active leg
//! ONCE; call sites stop re-stating the two-arm `match` per method
//! (dedup-at-the-seam, the `transport.rs` `ChatTransport` doctrine).

use serde::de::DeserializeOwned;

use crate::direct::DirectHttp;
use crate::gateway_api::{GatewayBlobClient, GatewayJsonClient};
use crate::relay;

pub(crate) enum ActiveGatewayClient {
    Direct(DirectHttp),
    Relay(relay::GatewayApi),
}

macro_rules! forward {
    ($self:ident, $client:ident => $call:expr) => {
        match $self {
            ActiveGatewayClient::Direct($client) => $call.await,
            ActiveGatewayClient::Relay($client) => $call.await,
        }
    };
}

impl GatewayJsonClient for ActiveGatewayClient {
    async fn get_json<'a, T>(&'a self, path: &'a str) -> Result<T, String>
    where
        T: DeserializeOwned + Send + 'static,
    {
        forward!(self, c => c.get_json::<T>(path))
    }

    async fn post_json<'a, T>(&'a self, path: &'a str, body: Vec<u8>) -> Result<T, String>
    where
        T: DeserializeOwned + Send + 'static,
    {
        forward!(self, c => c.post_json::<T>(path, body))
    }

    async fn post_empty<'a>(&'a self, path: &'a str, body: Vec<u8>) -> Result<(), String> {
        forward!(self, c => c.post_empty(path, body))
    }

    async fn put_empty<'a>(&'a self, path: &'a str, body: Vec<u8>) -> Result<(), String> {
        forward!(self, c => c.put_empty(path, body))
    }

    async fn delete_empty<'a>(&'a self, path: &'a str) -> Result<(), String> {
        forward!(self, c => c.delete_empty(path))
    }

    async fn post_raw<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
        retryable: bool,
    ) -> Result<Vec<u8>, String> {
        forward!(self, c => c.post_raw(path, body, retryable))
    }
}

impl GatewayBlobClient for ActiveGatewayClient {
    async fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
        deck_card: Option<String>,
    ) -> Result<String, String> {
        forward!(self, c => c.upload_blob(bytes, mime_type, deck_card))
    }

    async fn download_blob(
        &self,
        blob_id: String,
        progress: crate::blob_helper::ProgressSink,
    ) -> Result<Vec<u8>, String> {
        forward!(self, c => c.download_blob(blob_id, progress))
    }
}
