// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use api::v1::flow::flow_client::FlowClient as PbFlowClient;
use api::v1::health_check_client::HealthCheckClient;
use api::v1::prometheus_gateway_client::PrometheusGatewayClient;
use api::v1::region::region_client::RegionClient as PbRegionClient;
use api::v1::HealthCheckRequest;
use arrow_flight::flight_service_client::FlightServiceClient;
use common_grpc::channel_manager::{ChannelConfig, ChannelManager, ClientTlsOption};
use parking_lot::RwLock;
use snafu::{OptionExt, ResultExt};
use tonic::codec::CompressionEncoding;
use tonic::transport::Channel;

use crate::load_balance::{LoadBalance, Loadbalancer};
use crate::{error, Result};

pub struct FlightClient {
    addr: String,
    client: FlightServiceClient<Channel>,
}

impl FlightClient {
    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn mut_inner(&mut self) -> &mut FlightServiceClient<Channel> {
        &mut self.client
    }
}

#[derive(Clone, Debug, Default)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    channel_manager: ChannelManager,
    peers: Arc<RwLock<Vec<String>>>,
    load_balance: Loadbalancer,
}

impl Inner {
    fn with_manager(channel_manager: ChannelManager) -> Self {
        Self {
            channel_manager,
            ..Default::default()
        }
    }

    fn set_peers(&self, peers: Vec<String>) {
        let mut guard = self.peers.write();
        *guard = peers;
    }

    fn get_peer(&self) -> Option<String> {
        let guard = self.peers.read();
        self.load_balance.get_peer(&guard).cloned()
    }
}

impl Client {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_urls<U, A>(urls: A) -> Self
    where
        U: AsRef<str>,
        A: AsRef<[U]>,
    {
        Self::with_manager_and_urls(ChannelManager::new(), urls)
    }

    pub fn with_tls_and_urls<U, A>(urls: A, client_tls: ClientTlsOption) -> Result<Self>
    where
        U: AsRef<str>,
        A: AsRef<[U]>,
    {
        let channel_config = ChannelConfig::default().client_tls_config(client_tls);
        let channel_manager = ChannelManager::with_tls_config(channel_config)
            .context(error::CreateTlsChannelSnafu)?;
        Ok(Self::with_manager_and_urls(channel_manager, urls))
    }

    pub fn with_manager_and_urls<U, A>(channel_manager: ChannelManager, urls: A) -> Self
    where
        U: AsRef<str>,
        A: AsRef<[U]>,
    {
        let inner = Inner::with_manager(channel_manager);
        let urls: Vec<String> = urls
            .as_ref()
            .iter()
            .map(|peer| peer.as_ref().to_string())
            .collect();
        inner.set_peers(urls);
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn start<U, A>(&self, urls: A)
    where
        U: AsRef<str>,
        A: AsRef<[U]>,
    {
        let urls: Vec<String> = urls
            .as_ref()
            .iter()
            .map(|peer| peer.as_ref().to_string())
            .collect();

        self.inner.set_peers(urls);
    }

    pub fn find_channel(&self) -> Result<(String, Channel)> {
        let addr = self
            .inner
            .get_peer()
            .context(error::IllegalGrpcClientStateSnafu {
                err_msg: "No available peer found",
            })?;

        let channel = self
            .inner
            .channel_manager
            .get(&addr)
            .context(error::CreateChannelSnafu { addr: &addr })?;
        Ok((addr, channel))
    }

    pub fn max_grpc_recv_message_size(&self) -> usize {
        self.inner
            .channel_manager
            .config()
            .max_recv_message_size
            .as_bytes() as usize
    }

    pub fn max_grpc_send_message_size(&self) -> usize {
        self.inner
            .channel_manager
            .config()
            .max_send_message_size
            .as_bytes() as usize
    }

    pub fn make_flight_client(
        &self,
        send_compression: bool,
        accept_compression: bool,
    ) -> Result<FlightClient> {
        let (addr, channel) = self.find_channel()?;

        let mut client = FlightServiceClient::new(channel)
            .max_decoding_message_size(self.max_grpc_recv_message_size())
            .max_encoding_message_size(self.max_grpc_send_message_size());
        // todo(hl): support compression methods.
        if send_compression {
            client = client.send_compressed(CompressionEncoding::Zstd);
        }
        if accept_compression {
            client = client.accept_compressed(CompressionEncoding::Zstd);
        }

        Ok(FlightClient { addr, client })
    }

    pub(crate) fn raw_region_client(&self) -> Result<(String, PbRegionClient<Channel>)> {
        let (addr, channel) = self.find_channel()?;
        let client = PbRegionClient::new(channel)
            .max_decoding_message_size(self.max_grpc_recv_message_size())
            .max_encoding_message_size(self.max_grpc_send_message_size());
        Ok((addr, client))
    }

    pub(crate) fn raw_flow_client(&self) -> Result<(String, PbFlowClient<Channel>)> {
        let (addr, channel) = self.find_channel()?;
        let client = PbFlowClient::new(channel)
            .max_decoding_message_size(self.max_grpc_recv_message_size())
            .max_encoding_message_size(self.max_grpc_send_message_size())
            .accept_compressed(CompressionEncoding::Zstd)
            .send_compressed(CompressionEncoding::Zstd);
        Ok((addr, client))
    }

    pub fn make_prometheus_gateway_client(&self) -> Result<PrometheusGatewayClient<Channel>> {
        let (_, channel) = self.find_channel()?;
        let client = PrometheusGatewayClient::new(channel)
            .accept_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Zstd)
            .send_compressed(CompressionEncoding::Gzip)
            .send_compressed(CompressionEncoding::Zstd);
        Ok(client)
    }

    pub async fn health_check(&self) -> Result<()> {
        let (_, channel) = self.find_channel()?;
        let mut client = HealthCheckClient::new(channel);
        let _ = client.health_check(HealthCheckRequest {}).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::Inner;
    use crate::load_balance::Loadbalancer;

    fn mock_peers() -> Vec<String> {
        vec![
            "127.0.0.1:3001".to_string(),
            "127.0.0.1:3002".to_string(),
            "127.0.0.1:3003".to_string(),
        ]
    }

    #[test]
    fn test_inner() {
        let inner = Inner::default();

        assert!(matches!(
            inner.load_balance,
            Loadbalancer::Random(crate::load_balance::Random)
        ));
        assert!(inner.get_peer().is_none());

        let peers = mock_peers();
        inner.set_peers(peers.clone());
        let all: HashSet<String> = peers.into_iter().collect();

        for _ in 0..20 {
            assert!(all.contains(&inner.get_peer().unwrap()));
        }
    }
}
