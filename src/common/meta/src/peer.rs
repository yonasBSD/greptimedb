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

pub use api::v1::meta::Peer;

use crate::error::Error;
use crate::{DatanodeId, FlownodeId};

/// PeerLookupService is a service that can lookup peers.
#[async_trait::async_trait]
pub trait PeerLookupService {
    /// Returns the datanode with the given id. It may return inactive peers.
    async fn datanode(&self, id: DatanodeId) -> Result<Option<Peer>, Error>;

    /// Returns the flownode with the given id. It may return inactive peers.
    async fn flownode(&self, id: FlownodeId) -> Result<Option<Peer>, Error>;

    /// Returns all currently active frontend nodes that have reported a heartbeat within the most recent heartbeat interval from the in-memory backend.
    async fn active_frontends(&self) -> Result<Vec<Peer>, Error>;
}

pub type PeerLookupServiceRef = Arc<dyn PeerLookupService + Send + Sync>;
