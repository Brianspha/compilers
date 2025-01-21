use alloy_primitives::hex;
use foundry_compilers_artifacts::{resolc::ResolcCompilerOutput, SolcLanguage};
use md5::Digest;
use std::collections::{BTreeMap, HashSet};

use crate::{
    buildinfo::{BuildContext, RawBuildInfo, ETHERS_FORMAT_VERSION},
    compilers::resolc::ResolcVersionedInput,
    error::Result,
    CompilerInput,
};

pub mod contracts;
pub mod project;

pub fn raw_build_info_new(
    input: &ResolcVersionedInput,
    output: &ResolcCompilerOutput,
    full_build_info: bool,
) -> Result<RawBuildInfo<SolcLanguage>> {
    let version = input.solc_version.clone();
    let build_context = build_context_new(input, output)?;

    let mut hasher = md5::Md5::new();

    hasher.update(ETHERS_FORMAT_VERSION);

    let solc_short = format!("{}.{}.{}", version.major, version.minor, version.patch);
    hasher.update(&solc_short);
    hasher.update(version.to_string());

    let input = serde_json::to_value(input)?;
    hasher.update(&serde_json::to_string(&input)?);

    // create the hash for `{_format,solcVersion,solcLongVersion,input}`
    // N.B. this is not exactly the same as hashing the json representation of these values but
    // the must efficient one
    let result = hasher.finalize();
    let id = hex::encode(result);

    let mut build_info = BTreeMap::new();

    if full_build_info {
        build_info.insert("_format".to_string(), serde_json::to_value(ETHERS_FORMAT_VERSION)?);
        build_info.insert("solcVersion".to_string(), serde_json::to_value(&solc_short)?);
        build_info.insert("solcLongVersion".to_string(), serde_json::to_value(&version)?);
        build_info.insert("input".to_string(), input);
        build_info.insert("output".to_string(), serde_json::to_value(output)?);
    }

    Ok(RawBuildInfo { id, build_info, build_context })
}

pub fn build_context_new(
    input: &ResolcVersionedInput,
    output: &ResolcCompilerOutput,
) -> Result<BuildContext<SolcLanguage>> {
    let mut source_id_to_path = BTreeMap::new();

    let input_sources = input.sources().map(|(path, _)| path).collect::<HashSet<_>>();
    for (path, source) in output.sources.iter() {
        if input_sources.contains(path.as_path()) {
            source_id_to_path.insert(source.id, path.to_path_buf());
        }
    }

    Ok(BuildContext { source_id_to_path, language: input.language() })
}
