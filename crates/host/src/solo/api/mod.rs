use anyhow::Context as _;
use std::collections::HashMap;
use wasmcloud_core::InterfaceLinkDefinition;

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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LinkTarget {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) namespace: String,
    #[serde(default)]
    pub(crate) capability: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Link {
    #[serde(default = "default_link_name")]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) target: LinkTarget,
    #[serde(default)]
    pub(crate) wit_namespace: String,
    #[serde(default)]
    pub(crate) wit_package: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) interfaces: Vec<String>,
    #[serde(default)]
    pub(crate) config: HashMap<String, String>,
}

impl Link {
    pub fn as_import(
        &self,
        source_namespace: String,
        source_name: String,
        source_is_capability: bool,
    ) -> InterfaceLinkDefinition {
        let target = if self.target.capability {
            format!("capability.{}.{}", self.target.namespace, self.target.name)
        } else {
            format!("component.{}.{}", self.target.namespace, self.target.name)
        };

        let source_id = if source_is_capability {
            format!("capability.{}.{}", source_namespace, source_name)
        } else {
            format!("component.{}.{}", source_namespace, source_name)
        };

        InterfaceLinkDefinition {
            name: self.name.clone(),
            wit_namespace: self.wit_namespace.clone(),
            wit_package: self.wit_package.clone(),
            interfaces: self.interfaces.clone(),
            source_id,
            target,
            source_config: Default::default(),
            target_config: self.config.clone(),
            source_secrets: Default::default(),
            target_secrets: Default::default(),
        }
    }
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
    #[serde(default)]
    pub(crate) config: HashMap<String, String>,
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
    #[serde(default)]
    pub(crate) annotations: HashMap<String, String>,
    #[serde(default)]
    pub(crate) concurrency: u32,

    // Runtime Configuration
    #[serde(default)]
    pub(crate) wasi_config: HashMap<String, String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) imports: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) exports: Vec<Link>,
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
    #[serde(default)]
    pub(crate) annotations: HashMap<String, String>,
    #[serde(default)]
    pub(crate) config: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) imports: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) exports: Vec<Link>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StartProviderResponse {
    #[serde(default)]
    pub(crate) id: String,
}
