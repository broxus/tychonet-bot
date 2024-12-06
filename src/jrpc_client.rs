use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Context, Result};
use everscale_types::boc::{Boc, BocRepr};
use everscale_types::cell::{Cell, HashBytes};
use everscale_types::models::{Account, BlockchainConfig, StdAddr, Transaction};
use reqwest::{IntoUrl, Url};
use serde::{Deserialize, Serialize};

use crate::util::{serde_option_string, serde_string};

#[derive(Clone)]
#[repr(transparent)]
pub struct JrpcClient {
    inner: Arc<Inner>,
}

impl JrpcClient {
    pub fn new<U: IntoUrl>(endpoint: U) -> Result<Self> {
        let base_url = endpoint.into_url()?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let client = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .build()
            .context("failed to build http client")?;

        Ok(Self {
            inner: Arc::new(Inner { client, base_url }),
        })
    }

    pub async fn get_timings(&self) -> Result<StateTimings> {
        self.inner
            .post(&JrpcRequest {
                method: "getTimings",
                params: &(),
            })
            .await
    }

    pub async fn get_account(&self, address: &StdAddr) -> Result<AccountStateResponse> {
        #[derive(Serialize)]
        struct Params<'a> {
            address: &'a StdAddr,
        }

        self.inner
            .post(&JrpcRequest {
                method: "getContractState",
                params: &Params { address },
            })
            .await
    }

    #[allow(unused)]
    pub async fn get_dst_transaction(
        &self,
        message_hash: &HashBytes,
    ) -> Result<Option<Box<Transaction>>> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Params<'a> {
            message_hash: &'a HashBytes,
        }

        let Some(tx) = self
            .inner
            .post::<_, Option<String>>(&JrpcRequest {
                method: "getDstTransaction",
                params: &Params { message_hash },
            })
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(BocRepr::decode_base64(&tx)?))
    }

    #[allow(unused)]
    pub async fn get_transactions(
        &self,
        address: &StdAddr,
        last_lt: Option<u64>,
        limit: u8,
    ) -> Result<Vec<Box<Transaction>>> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Params<'a> {
            pub account: &'a StdAddr,
            #[serde(with = "serde_option_string")]
            pub last_transaction_lt: Option<u64>,
            pub limit: u8,
        }

        let txs = self
            .inner
            .post::<_, Vec<String>>(&JrpcRequest {
                method: "getTransactionsList",
                params: &Params {
                    account: address,
                    last_transaction_lt: last_lt,
                    limit,
                },
            })
            .await?;

        txs.into_iter()
            .map(|tx| BocRepr::decode_base64(&tx))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub async fn get_config(&self) -> Result<LatestBlockchainConfig> {
        self.inner
            .post(&JrpcRequest {
                method: "getBlockchainConfig",
                params: &(),
            })
            .await
    }
}

struct Inner {
    client: reqwest::Client,
    base_url: Url,
}

impl Inner {
    async fn post<Q, R>(&self, data: &Q) -> Result<R>
    where
        Q: Serialize,
        for<'de> R: Deserialize<'de>,
    {
        let response = self
            .client
            .post(self.base_url.clone())
            .json(data)
            .send()
            .await?;

        let res = response.text().await?;
        tracing::info!(res);

        match serde_json::from_str(&res)? {
            JrpcResponse::Success(res) => Ok(res),
            JrpcResponse::Err(err) => anyhow::bail!(err),
        }
    }
}

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateTimings {
    pub last_mc_block_seqno: u32,
    pub last_mc_utime: u32,
    pub mc_time_diff: i64,
    pub smallest_known_lt: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestBlockchainConfig {
    pub global_id: i32,
    pub seqno: u32,
    #[serde(with = "BocRepr")]
    pub config: BlockchainConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[allow(unused)]
pub enum AccountStateResponse {
    NotExists {
        timings: GenTimings,
    },
    #[serde(rename_all = "camelCase")]
    Exists {
        #[serde(deserialize_with = "deserialize_account")]
        account: Box<Account>,
        timings: GenTimings,
        last_transaction_id: LastTransactionId,
    },
    Unchanged {
        timings: GenTimings,
    },
}

fn deserialize_account<'de, D>(deserializer: D) -> Result<Box<Account>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use everscale_types::cell::Load;
    use serde::de::Error;

    fn read_account(cell: Cell) -> Result<Box<Account>, everscale_types::error::Error> {
        let s = &mut cell.as_slice()?;
        Ok(Box::new(Account {
            address: <_>::load_from(s)?,
            storage_stat: <_>::load_from(s)?,
            last_trans_lt: <_>::load_from(s)?,
            balance: <_>::load_from(s)?,
            state: <_>::load_from(s)?,
            init_code_hash: if s.is_data_empty() {
                None
            } else {
                Some(<_>::load_from(s)?)
            },
        }))
    }

    Boc::deserialize(deserializer).and_then(|cell| read_account(cell).map_err(Error::custom))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenTimings {
    #[serde(with = "serde_string")]
    pub gen_lt: u64,
    pub gen_utime: u32,
}

#[derive(Deserialize)]
#[allow(unused)]
pub struct LastTransactionId {
    #[serde(with = "serde_string")]
    pub lt: u64,
    pub hash: HashBytes,
}

struct JrpcRequest<'a, T> {
    method: &'a str,
    params: &'a T,
}

impl<T: Serialize> Serialize for JrpcRequest<'_, T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut ser = serializer.serialize_struct("JrpcRequest", 4)?;
        ser.serialize_field("jsonrpc", "2.0")?;
        ser.serialize_field("id", &1)?;
        ser.serialize_field("method", self.method)?;
        ser.serialize_field("params", self.params)?;
        ser.end()
    }
}

enum JrpcResponse<T> {
    Success(T),
    Err(Box<serde_json::value::RawValue>),
}

impl<'de, T> Deserialize<'de> for JrpcResponse<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(de: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Field {
            Result,
            Error,
            #[serde(other)]
            Other,
        }

        enum ResponseData<T> {
            Result(T),
            Error(Box<serde_json::value::RawValue>),
        }

        struct ResponseVisitor<T>(PhantomData<T>);

        impl<'de, T> serde::de::Visitor<'de> for ResponseVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = ResponseData<T>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON-RPC response object")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut result = None::<ResponseData<T>>;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Result if result.is_none() => {
                            result = Some(map.next_value().map(ResponseData::Result)?);
                        }
                        Field::Error if result.is_none() => {
                            result = Some(map.next_value().map(ResponseData::Error)?);
                        }
                        Field::Other => {
                            map.next_value::<&serde_json::value::RawValue>()?;
                        }
                        Field::Result => return Err(serde::de::Error::duplicate_field("result")),
                        Field::Error => return Err(serde::de::Error::duplicate_field("error")),
                    }
                }

                result.ok_or_else(|| serde::de::Error::missing_field("result or error"))
            }
        }

        Ok(match de.deserialize_map(ResponseVisitor(PhantomData))? {
            ResponseData::Result(result) => JrpcResponse::Success(result),
            ResponseData::Error(error) => JrpcResponse::Err(error),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde() {
        let json = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}";
        serde_json::from_str::<JrpcResponse<()>>(json).unwrap();

        let json = "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"unknown method\"}}";
        serde_json::from_str::<JrpcResponse<()>>(json).unwrap();

        let json = "{\"jsonrpc\":\"2.0\",\"result\":42,\"id\":1}";
        serde_json::from_str::<JrpcResponse<i32>>(json).unwrap();
    }
}
