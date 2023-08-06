pub mod error;

use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use dbeel::shards::{hash_string, ClusterMetadata};
use futures_lite::{AsyncReadExt, AsyncWriteExt};
use glommio::net::TcpStream;
use rmp_serde::from_slice;
use rmpv::{encode::write_value, Integer, Utf8String, Value};

use crate::error::{Error, Result};

struct Shard {
    hash: u32,
    address: SocketAddr,
}

pub struct DbeelClient {
    seed_shards: Vec<SocketAddr>,
    hash_ring: Vec<Shard>,
    replication_factor: u32,
}

pub struct Collection {
    client: Arc<DbeelClient>,
    name: Utf8String,
}

fn to_utf8string<S: Into<Utf8String>>(
    maybe_utf8string: S,
) -> Result<Utf8String> {
    let utf8 = maybe_utf8string.into();
    if !utf8.is_str() {
        return Err(Error::InvalidUtf8String(utf8));
    }
    Ok(utf8)
}

impl DbeelClient {
    pub async fn from_seed_nodes<A>(addresses: &[A]) -> Result<Arc<Self>>
    where
        A: ToSocketAddrs,
    {
        let mut seed_addresses = vec![];
        for address in addresses {
            match address.to_socket_addrs() {
                Ok(addrs) => seed_addresses.extend(addrs),
                Err(e) => return Err(Error::ParsingSocketAddress(e)),
            };
        }

        let request = Value::Map(vec![(
            Value::String("type".into()),
            Value::String("get_cluster_metadata".into()),
        )]);
        let buf = Self::send_request(&seed_addresses, request).await?;
        let metadata: ClusterMetadata = from_slice(&buf)?;
        let flatten_shards = metadata
            .nodes
            .into_iter()
            .map(|node| format!("{}:{}", node.ip, node.db_port))
            .flat_map(|address| {
                address
                    .to_socket_addrs()
                    .map(|socket_addr| {
                        let hash =
                            hash_string(&address).map_err(Error::HashShardName);
                        (hash, socket_addr)
                    })
                    .map_err(Error::ParsingSocketAddress)
            })
            .collect::<Vec<(Result<u32>, std::vec::IntoIter<SocketAddr>)>>();

        let mut hash_ring = Vec::new();
        for (hash_result, socket_addrs) in flatten_shards {
            let hash = hash_result?;
            for address in socket_addrs {
                hash_ring.push(Shard { hash, address });
            }
        }
        hash_ring.sort_unstable_by_key(|s| s.hash);

        Ok(Arc::new(Self {
            seed_shards: seed_addresses,
            hash_ring,
            replication_factor: metadata.replication_factor,
        }))
    }

    pub fn collection<S: Into<Utf8String>>(
        self: Arc<Self>,
        name: S,
    ) -> Collection {
        Collection {
            client: self,
            name: name.into(),
        }
    }

    async fn send_buffer(
        address: &SocketAddr,
        buffer: &Vec<u8>,
    ) -> Result<Vec<u8>> {
        let mut stream = TcpStream::connect(address)
            .await
            .map_err(Error::ConnectToShard)?;

        let size_buffer = (buffer.len() as u16).to_le_bytes();
        stream
            .write_all(&size_buffer)
            .await
            .map_err(Error::CommunicateWithShard)?;
        stream
            .write_all(buffer)
            .await
            .map_err(Error::CommunicateWithShard)?;

        let mut response_buffer = Vec::new();
        stream
            .read_to_end(&mut response_buffer)
            .await
            .map_err(Error::CommunicateWithShard)?;

        Ok(response_buffer)
    }

    pub(crate) async fn send_request(
        addresses: &[SocketAddr],
        request: Value,
    ) -> Result<Vec<u8>> {
        if addresses.is_empty() {
            return Err(Error::NoAddresses);
        }

        let mut data_encoded: Vec<u8> = Vec::new();
        write_value(&mut data_encoded, &request)?;

        let mut errors = vec![];
        for address in addresses {
            match Self::send_buffer(address, &data_encoded).await {
                Ok(response_encoded) => {
                    return Ok(response_encoded);
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }

        Err(Error::SendRequestToCluster(errors))
    }

    pub(crate) async fn send_sharded_request(
        &self,
        shard_key: &String,
        request: Value,
    ) -> Result<Vec<u8>> {
        let hash = hash_string(shard_key).map_err(Error::HashShardName)?;
        let position = self
            .hash_ring
            .iter()
            .position(|s| s.hash >= hash)
            .unwrap_or(0);

        let mut owning_shards = Vec::new();
        for i in 0..self.replication_factor {
            let index = (position + i as usize) % self.hash_ring.len();
            if i > 0 && index == position {
                break;
            }
            owning_shards.push(self.hash_ring[index].address);
        }
        Ok(DbeelClient::send_request(&owning_shards, request).await?)
    }

    pub async fn create_collection<S: Into<Utf8String>>(
        self: Arc<Self>,
        name: S,
    ) -> Result<Collection> {
        let name = to_utf8string(name)?;
        let request = Value::Map(vec![
            (
                Value::String("type".into()),
                Value::String("create_collection".into()),
            ),
            (Value::String("name".into()), Value::String(name.clone())),
        ]);
        Self::send_request(&self.seed_shards, request).await?;

        Ok(self.collection(name))
    }

    pub(crate) async fn drop_collection<S: Into<Utf8String>>(
        self: Arc<Self>,
        name: S,
    ) -> Result<()> {
        let name = to_utf8string(name)?;
        let request = Value::Map(vec![
            (
                Value::String("type".into()),
                Value::String("drop_collection".into()),
            ),
            (Value::String("name".into()), Value::String(name.clone())),
        ]);
        Self::send_request(&self.seed_shards, request).await?;
        Ok(())
    }
}

impl Collection {
    pub async fn get_consistent<S, I>(
        &self,
        key: S,
        consistency: I,
    ) -> Result<Vec<u8>>
    where
        S: Into<Utf8String>,
        I: Into<Integer>,
    {
        let key = to_utf8string(key)?;
        let request = Value::Map(vec![
            (Value::String("type".into()), Value::String("get".into())),
            (Value::String("key".into()), Value::String(key.clone())),
            (
                Value::String("collection".into()),
                Value::String(self.name.clone()),
            ),
            (
                Value::String("consistency".into()),
                Value::Integer(consistency.into()),
            ),
        ]);
        self.client
            .send_sharded_request(&(key.into_str().unwrap()), request)
            .await
    }

    pub async fn get<S>(&self, key: S) -> Result<Vec<u8>>
    where
        S: Into<Utf8String>,
    {
        self.get_consistent(key, 1).await
    }

    pub async fn set_consistent<S, I>(
        &self,
        key: S,
        value: Value,
        consistency: I,
    ) -> Result<Vec<u8>>
    where
        S: Into<Utf8String>,
        I: Into<Integer>,
    {
        let key = to_utf8string(key)?;
        let request = Value::Map(vec![
            (Value::String("type".into()), Value::String("set".into())),
            (Value::String("key".into()), Value::String(key.clone())),
            (Value::String("value".into()), value),
            (
                Value::String("collection".into()),
                Value::String(self.name.clone()),
            ),
            (
                Value::String("consistency".into()),
                Value::Integer(consistency.into()),
            ),
        ]);
        self.client
            .send_sharded_request(&(key.into_str().unwrap()), request)
            .await
    }

    pub async fn set<S: Into<Utf8String>>(
        &self,
        key: S,
        value: Value,
    ) -> Result<Vec<u8>> {
        self.set_consistent(key, value, 1).await
    }

    pub async fn delete<S: Into<Utf8String>>(&self, key: S) -> Result<Vec<u8>> {
        let key = to_utf8string(key)?;
        let request = Value::Map(vec![
            (Value::String("type".into()), Value::String("delete".into())),
            (Value::String("key".into()), Value::String(key.clone())),
            (
                Value::String("collection".into()),
                Value::String(self.name.clone()),
            ),
        ]);
        self.client
            .send_sharded_request(&(key.into_str().unwrap()), request)
            .await
    }

    pub async fn drop(self) -> Result<()> {
        self.client.drop_collection(self.name).await
    }
}
