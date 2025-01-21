use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

pub mod contract;
use contract::ResolcContract;
use foundry_compilers_artifacts_solc::{
    Bytecode, DeployedBytecode, Error, FileToContractsMap, SourceFile, SourceFiles,
};
use serde::{Deserialize, Serialize};

/// This file contains data structures that we need defined locally as some of them need to be used in trait
/// Implementation in such a way that they are owned so if we use existing structures from Revive
/// We will run into issues

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ResolcCompilerOutput {
    /// The file-contract hashmap.
    #[serde(default)]
    pub contracts: FileToContractsMap<ResolcContract>,
    /// The source code mapping data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<PathBuf, SourceFile>,
    /// The compilation errors and warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Error>,
    /// The `solc` compiler version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The `solc` compiler long version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_version: Option<String>,
    /// The `resolc` compiler version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revive_version: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecursiveFunction {
    /// The function name.
    pub name: String,
    /// The creation code function block tag.
    pub creation_tag: Option<usize>,
    /// The runtime code function block tag.
    pub runtime_tag: Option<usize>,
    /// The number of input arguments.
    #[serde(rename = "totalParamSize")]
    pub input_size: usize,
    /// The number of output arguments.
    #[serde(rename = "totalRetParamSize")]
    pub output_size: usize,
}
#[derive(Debug, Default, Serialize, Deserialize, Clone, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtraMetadata {
    /// The list of recursive functions.
    #[serde(default = "Vec::new")]
    pub recursive_functions: Vec<RecursiveFunction>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ResolcEVM {
    /// The contract EVM legacy assembly code.
    #[serde(rename = "legacyAssembly", skip_serializing_if = "Option::is_none")]
    pub assembly: Option<serde_json::Value>,
    /// The contract PolkaVM assembly code.
    #[serde(rename = "assembly", skip_serializing_if = "Option::is_none")]
    pub assembly_text: Option<String>,
    /// The contract bytecode.
    /// Is reset by that of PolkaVM before yielding the compiled project artifacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytecode: Option<Bytecode>,
    /// The deployed bytecode of the contract.
    /// It is overwritten with the PolkaVM blob before yielding the compiled project artifacts.
    /// Hence it will be the same as the runtime code but we keep both for compatibility reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed_bytecode: Option<DeployedBytecode>,
    /// The contract function signatures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_identifiers: Option<BTreeMap<String, String>>,
    /// The extra EVMLA metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_metadata: Option<ExtraMetadata>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EVM {
    /// The contract EraVM assembly code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly: Option<String>,
    /// The contract EVM legacy assembly code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_assembly: Option<serde_json::Value>,
    /// The contract bytecode.
    /// Is reset by that of EraVM before yielding the compiled project artifacts.
    pub bytecode: Option<Bytecode>,
    /// The list of function hashes
    #[serde(default, skip_serializing_if = "::std::collections::BTreeMap::is_empty")]
    pub method_identifiers: BTreeMap<String, String>,
    /// The extra EVMLA metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_metadata: Option<ExtraMetadata>,
}
pub type ResolcContracts = FileToContractsMap<ResolcContract>;

/// A wrapper helper type for the `Contracts` type alias
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutputContracts(pub ResolcContracts);
impl ResolcCompilerOutput {
    /// Whether the output contains a compiler error
    pub fn has_error(&self) -> bool {
        self.errors.iter().any(|err| err.severity.is_error())
    }

    /// Returns the output's source files and contracts separately, wrapped in helper types that
    /// provide several helper methods
    pub fn split(self) -> (SourceFiles, OutputContracts) {
        (SourceFiles(self.sources), OutputContracts(self.contracts))
    }

    /// Retains only those files the given iterator yields
    ///
    /// In other words, removes all contracts for files not included in the iterator
    pub fn retain_files<'a, I>(&mut self, files: I)
    where
        I: IntoIterator<Item = &'a Path>,
    {
        // Note: use `to_lowercase` here because solc not necessarily emits the exact file name,
        // e.g. `src/utils/upgradeProxy.sol` is emitted as `src/utils/UpgradeProxy.sol`
        let files: HashSet<_> =
            files.into_iter().map(|s| s.to_string_lossy().to_lowercase()).collect();
        self.contracts.retain(|f, _| files.contains(&f.to_string_lossy().to_lowercase()));
        self.sources.retain(|f, _| files.contains(&f.to_string_lossy().to_lowercase()));
    }

    pub fn merge(&mut self, other: Self) {
        self.errors.extend(other.errors);
        self.contracts.extend(other.contracts);
        self.sources.extend(other.sources);
    }

    pub fn join_all(&mut self, root: impl AsRef<Path>) {
        let root = root.as_ref();
        self.contracts = std::mem::take(&mut self.contracts)
            .into_iter()
            .map(|(path, contracts)| (root.join(path), contracts))
            .collect();
        self.sources = std::mem::take(&mut self.sources)
            .into_iter()
            .map(|(path, source)| (root.join(path), source))
            .collect();
    }
}
