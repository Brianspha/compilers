use crate::{
    artifacts::{serde_helpers, EvmVersion, Libraries},
    compilers::{restrictions::CompilerSettingsRestrictions, CompilerSettings},
    solc, OutputSelection,
};
use alloy_primitives::map::HashMap;
use foundry_compilers_artifacts::Remapping;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::compiler::ResolcCliSettings;

/// This file contains functionality required by revive/resolc
/// Some functions are stubbed but will be implemented as needed
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolcOptimizer {
    pub enabled: bool,
    #[serde(default)]
    pub runs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub struct ResolcSettings {
    pub optimizer: ResolcOptimizer,
    #[serde(rename = "outputSelection")]
    pub outputselection: HashMap<String, HashMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remappings: Vec<Remapping>,
    #[serde(default)]
    pub libraries: Libraries,
    #[serde(skip)]
    pub resolc_settings: ResolcCliSettings,
    #[serde(
        default,
        with = "serde_helpers::display_from_str_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub evm_version: Option<EvmVersion>,
}

#[derive(Debug, Clone, Eq, PartialEq, Copy)]
pub enum ResolcRestrictions {
    Default,
}

impl Default for ResolcRestrictions {
    fn default() -> Self {
        Self::Default
    }
}
impl Default for ResolcOptimizer {
    fn default() -> Self {
        Self { enabled: false, runs: 200 }
    }
}

impl CompilerSettingsRestrictions for ResolcRestrictions {
    fn merge(self, _other: Self) -> Option<Self> {
        Some(self)
    }
}

impl CompilerSettings for ResolcSettings {
    type Restrictions = ResolcRestrictions;

    fn update_output_selection(
        &mut self,
        f: impl FnOnce(&mut foundry_compilers_artifacts::output_selection::OutputSelection) + Copy,
    ) {
        // Here we will just include all output selection types
        // In the future we could include the neccesary i.e. default to reduce
        // The size of the output file
        let mut output_selection =
            foundry_compilers_artifacts::output_selection::OutputSelection::complete_output_selection();
        f(&mut output_selection);

        let mut selection = HashMap::default();

        for (file, contracts) in output_selection.0 {
            let mut file_outputs = HashMap::default();
            for (contract, outputs) in contracts {
                file_outputs.insert(contract, outputs.into_iter().collect());
            }
            selection.insert(file, file_outputs);
        }

        self.outputselection = selection;
    }

    fn can_use_cached(&self, other: &Self) -> bool {
        self.optimizer == other.optimizer && self.outputselection == other.outputselection
    }

    fn satisfies_restrictions(&self, restrictions: &Self::Restrictions) -> bool {
        match restrictions {
            ResolcRestrictions::Default => true,
        }
    }

    fn with_remappings(self, remappings: &[Remapping]) -> Self {
        Self { remappings: remappings.to_vec(), ..self }
    }

    fn with_base_path(self, base_path: &Path) -> Self {
        Self {
            resolc_settings: ResolcCliSettings {
                base_path: Some(base_path.to_path_buf()),
                ..self.resolc_settings
            },
            ..self
        }
    }

    fn with_allow_paths(self, allow_paths: &BTreeSet<PathBuf>) -> Self {
        Self {
            resolc_settings: ResolcCliSettings {
                allow_paths: allow_paths.clone(),
                ..self.resolc_settings
            },
            ..self
        }
    }

    fn with_include_paths(self, include_paths: &BTreeSet<PathBuf>) -> Self {
        Self {
            resolc_settings: ResolcCliSettings {
                include_paths: include_paths.clone(),
                ..self.resolc_settings
            },
            ..self
        }
    }
}

impl ResolcOptimizer {
    pub fn new(enabled: bool, runs: u64) -> Self {
        Self { enabled, runs }
    }
}
impl ResolcSettings {
    pub fn new(
        optimizer: ResolcOptimizer,
        output_selection: HashMap<String, HashMap<String, Vec<String>>>,
        resolc_settings: ResolcCliSettings,
        remappings: Vec<Remapping>,
        evm_version: Option<EvmVersion>,
        libraries: Libraries,
    ) -> Self {
        Self {
            optimizer,
            outputselection: output_selection,
            resolc_settings,
            remappings,
            evm_version,
            libraries,
        }
    }
    pub fn strip_prefix(&mut self, base: impl AsRef<Path>) {
        let base = base.as_ref();
        self.remappings.iter_mut().for_each(|r| {
            r.strip_prefix(base);
        });

        self.libraries.libs = std::mem::take(&mut self.libraries.libs)
            .into_iter()
            .map(|(file, libs)| (file.strip_prefix(base).map(Into::into).unwrap_or(file), libs))
            .collect();
    }
}
