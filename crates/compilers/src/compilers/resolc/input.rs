use alloy_primitives::map::HashMap;
use foundry_compilers_artifacts::{SolcLanguage, Source, Sources};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::CompilerInput;

use super::ResolcSettings;

#[derive(Debug, Clone, Serialize)]
pub struct ResolcVersionedInput {
    #[serde(flatten)]
    pub input: ResolcInput,
    pub solc_version: Version,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolcInput {
    pub language: SolcLanguage,
    pub sources: Sources,
    pub settings: ResolcSettings,
}

impl Default for ResolcInput {
    fn default() -> Self {
        Self {
            language: SolcLanguage::Solidity,
            sources: Sources::default(),
            settings: ResolcSettings::default(),
        }
    }
}

impl CompilerInput for ResolcVersionedInput {
    type Settings = ResolcSettings;
    type Language = SolcLanguage;

    fn build(
        sources: Sources,
        settings: Self::Settings,
        language: Self::Language,
        version: Version,
    ) -> Self {
        let input = ResolcInput::new(language, sources, settings);
        Self { input, solc_version: version }
    }

    fn language(&self) -> Self::Language {
        self.input.language
    }

    fn version(&self) -> &Version {
        &self.solc_version
    }

    fn sources(&self) -> impl Iterator<Item = (&Path, &Source)> {
        self.input.sources.iter().map(|(path, source)| (path.as_path(), source))
    }

    fn compiler_name(&self) -> std::borrow::Cow<'static, str> {
        "resolc".into()
    }

    fn strip_prefix(&mut self, base: &Path) {
        self.input.strip_prefix(base);
    }
}

impl ResolcInput {
    fn new(language: SolcLanguage, sources: Sources, settings: ResolcSettings) -> Self {
        Self { language, sources, settings }
    }
    pub fn strip_prefix(&mut self, base: impl AsRef<Path>) {
        let base = base.as_ref();
        self.sources = std::mem::take(&mut self.sources)
            .into_iter()
            .map(|(path, s)| (path.strip_prefix(base).map(Into::into).unwrap_or(path), s))
            .collect();

        self.settings.strip_prefix(base);
    }
}
