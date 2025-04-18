use anyhow::Context as _;
use std::collections::BTreeMap;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Response<T> {
    #[serde(default)]
    pub(crate) success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, flatten)]
    pub(crate) payload: T,
}

impl<T> Response<T> {
    pub fn success(payload: T) -> Self {
        Self {
            success: true,
            payload,
            message: None,
        }
    }

    pub fn error(payload: T) -> Self {
        Self {
            success: false,
            payload,
            message: None,
        }
    }

    pub fn success_with_message(message: String, payload: T) -> Self {
        Self {
            success: true,
            payload,
            message: Some(message),
        }
    }

    pub fn error_with_message(message: String, payload: T) -> Self {
        Self {
            success: false,
            payload,
            message: Some(message),
        }
    }

    pub fn serialize(&self) -> anyhow::Result<Vec<u8>>
    where
        T: Serialize,
    {
        serde_json::to_vec(self).context("failed to serialize response")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Request<T> {
    #[serde(default, flatten)]
    pub(crate) payload: T,
}

impl<T> Request<T> {
    pub fn deserialize<'a>(payload: &'a [u8]) -> anyhow::Result<Self>
    where
        T: Default + Deserialize<'a>,
    {
        let request = serde_json::from_slice::<Request<T>>(payload)
            .context("failed to deserialize component start command")?;
        Ok(request)
    }
}

impl<T> TryFrom<&[u8]> for Request<T>
where
    T: Default + DeserializeOwned,
{
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Self::deserialize(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize, Hash)]
#[non_exhaustive]
pub struct Link {
    /// Target for the link, which can be a unique identifier or (future) a routing group
    #[serde(default)]
    pub(crate) target: String,
    /// Name of the link. Not providing this is equivalent to specifying "default"
    #[serde(default = "default_link_name")]
    pub(crate) name: String,
    /// WIT namespace of the link operation, e.g. `wasi` in `wasi:keyvalue/readwrite.get`
    #[serde(default)]
    pub(crate) wit_namespace: String,
    /// WIT package of the link operation, e.g. `keyvalue` in `wasi:keyvalue/readwrite.get`
    #[serde(default)]
    pub(crate) wit_package: String,
    /// WIT Interfaces to be used for the link, e.g. `readwrite`, `atomic`, etc.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) interfaces: Vec<String>,
    /// List of named configurations to provide to the target upon request
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<BTreeMap<String, String>>,
}

fn default_link_name() -> String {
    "default".to_string()
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Capability {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<BTreeMap<String, String>>,
}

/// Command a host to scale a component
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StartComponentRequest {
    // Identity & Resources
    #[serde(default)]
    pub(crate) namespace: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) annotations: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub(crate) concurrency: u32,

    // Runtime Configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wasi_config: Option<BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) links: Vec<Link>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StartComponentResponse {
    #[serde(default)]
    pub(crate) id: String,
}

/// Command a host to stop a component
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StopComponentRequest {
    #[serde(default)]
    pub(crate) id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StopComponentResponse {}

/// Command a host to scale a component
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StartProviderRequest {
    // Identity & Resources
    #[serde(default)]
    pub(crate) namespace: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) annotations: Option<BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StartProviderResponse {
    #[serde(default)]
    pub(crate) id: String,
}
