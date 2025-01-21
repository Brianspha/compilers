use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
};

use alloy_json_abi::JsonAbi;
use foundry_compilers_artifacts_solc::{
    CompactBytecode, CompactContractBytecode, CompactContractBytecodeCow, CompactContractRef,
    CompactDeployedBytecode, DevDoc, StorageLayout, UserDoc,
};
use serde::{Deserialize, Serialize};

use crate::{ResolcEVM, EVM};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResolcContract {
    /// The contract ABI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<JsonAbi>,
    /// The contract metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// The contract developer documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devdoc: Option<DevDoc>,
    /// The contract user documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub userdoc: Option<UserDoc>,
    /// The contract storage layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_layout: Option<StorageLayout>,
    /// Contract's bytecode and related objects
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evm: Option<EVM>,
    /// Revive related output
    /// We are going to use  structs defined locally
    /// as opposed to revive defined
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolc_evm: Option<ResolcEVM>,
    /// The contract optimized IR code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ir_optimized: Option<String>,
    /// The contract PolkaVM bytecode hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// The contract factory dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_dependencies: Option<BTreeMap<String, String>>,
    /// The contract missing libraries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_libraries: Option<HashSet<String>>,
}

impl Default for ResolcContract {
    fn default() -> Self {
        Self {
            abi: None,
            metadata: None,
            devdoc: None,
            userdoc: None,
            storage_layout: None,
            evm: None,
            ir_optimized: None,
            hash: None,
            factory_dependencies: None,
            missing_libraries: None,
            resolc_evm: None,
        }
    }
}

impl<'a> From<&'a ResolcContract> for CompactContractBytecodeCow<'a> {
    fn from(value: &'a ResolcContract) -> Self {
        if let Some((standard_abi, compact_bytecode, compact_deployed_bytecode)) =
            create_compact_bytecode(value)
        {
            Self {
                abi: Some(Cow::Owned(standard_abi)),
                bytecode: Some(Cow::Owned(compact_bytecode)),
                deployed_bytecode: Some(Cow::Owned(compact_deployed_bytecode)),
            }
        } else {
            Self { abi: None, bytecode: None, deployed_bytecode: None }
        }
    }
}

impl From<ResolcContract> for CompactContractBytecode {
    fn from(value: ResolcContract) -> Self {
        if let Some((standard_abi, compact_bytecode, compact_deployed_bytecode)) =
            create_compact_bytecode(&value)
        {
            Self {
                abi: Some(standard_abi),
                bytecode: Some(compact_bytecode),
                deployed_bytecode: Some(compact_deployed_bytecode),
            }
        } else {
            Self { abi: None, bytecode: None, deployed_bytecode: None }
        }
    }
}

impl<'a> From<&'a ResolcContract> for CompactContractRef<'a> {
    fn from(c: &'a ResolcContract) -> Self {
        let (bin, bin_runtime) = if let Some(ref evm) = c.resolc_evm {
            (
                evm.bytecode.as_ref().map(|code| &code.object),
                evm.deployed_bytecode
                    .as_ref()
                    .and_then(|deployed| deployed.bytecode.as_ref().map(|code| &code.object)),
            )
        } else {
            (None, None)
        };

        Self { abi: c.abi.as_ref(), bin, bin_runtime }
    }
}
fn create_compact_bytecode(
    parent_contract: &ResolcContract,
) -> Option<(JsonAbi, CompactBytecode, CompactDeployedBytecode)> {
    let Some(resolc_evm) = &parent_contract.resolc_evm else {
        return None;
    };

    let Some(bytecode) = &resolc_evm.bytecode else {
        return None;
    };

    let Some(deployed) = &resolc_evm.deployed_bytecode else {
        return None;
    };

    let Some(deployed_bytecode) = &deployed.bytecode else {
        return None;
    };

    let compact_bytecode = CompactBytecode {
        object: bytecode.object.clone(),
        source_map: None,
        link_references: BTreeMap::default(),
    };

    let compact_bytecode_deployed = CompactBytecode {
        object: deployed_bytecode.object.clone(),
        source_map: None,
        link_references: BTreeMap::default(),
    };

    let compact_deployed_bytecode = CompactDeployedBytecode {
        bytecode: Some(compact_bytecode_deployed),
        immutable_references: BTreeMap::default(),
    };

    Some((
        parent_contract.abi.clone().unwrap_or_default(),
        compact_bytecode,
        compact_deployed_bytecode,
    ))
}
