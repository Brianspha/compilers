use crate::{
    error::{Result, SolcError},
    resolver::parse::SolData,
    solc::{Solc, SolcCompiler, SolcSettings},
    Compiler, CompilerVersion,
};
use foundry_compilers_artifacts::{resolc::ResolcCompilerOutput, Contract, Error, SolcLanguage};
use itertools::Itertools;
use semver::Version;
use serde::Serialize;
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
};

use super::{ResolcInput, ResolcVersionedInput};

/// resolc and solc version may not be read anywhere in this code but
/// I forsee their use elswhere in the foundry project
/// So for now we keep them if needed we can remove them in future
/// Iterations
#[derive(Clone, Debug)]
pub struct Resolc {
    pub resolc: PathBuf,
    pub solc: SolcCompiler,
}

impl Compiler for Resolc {
    type CompilerContract = Contract;
    type Input = ResolcVersionedInput;
    type CompilationError = Error;
    type ParsedSource = SolData;
    type Settings = SolcSettings;
    type Language = SolcLanguage;

    /// Instead of using specific sols version we are going to autodetect
    /// Installed versions
    fn available_versions(&self, language: &SolcLanguage) -> Vec<CompilerVersion> {
        self.solc.available_versions(language)
    }

    fn compile(
        &self,
        input: &Self::Input,
    ) -> Result<crate::compilers::CompilerOutput<Error, Self::CompilerContract>, SolcError> {
        let solc = self.solc(input)?;
        let results = self.compile_output::<ResolcInput>(&solc, &input.input)?;
        let output = std::str::from_utf8(&results).map_err(|_| SolcError::InvalidUtf8)?;

        let results: ResolcCompilerOutput =
            serde_json::from_str(output).map_err(|e| SolcError::msg(e.to_string()))?;
        Ok(results.into())
    }
}

impl Default for Resolc {
    fn default() -> Self {
        #[cfg(feature = "svm-solc")]
        let solc = SolcCompiler::AutoDetect;
        #[cfg(not(feature = "svm-solc"))]
        let solc = crate::solc::Solc::new("solc")
            .map(SolcCompiler::Specific)
            .ok()
            .expect("Solc binary must be already installed");

        Self { resolc: which::which("resolc").expect("Resolc binary must be installed."), solc }
    }
}

impl Resolc {
    /// When creating a new Resolc Compiler instance for now we only care for
    /// Passing in the path to resolc but i do see a need perhaps once we get
    /// Things working to allow for passing in a custom solc path since revive
    /// Does allow for specifying a custom path for a solc bin
    /// Current impl just checks if theres any solc version installed if not
    /// We install but as mentioned this could change as it may not be the best
    /// approach since requirements are going to change
    pub fn new(revive_path: PathBuf, solc_compiler: SolcCompiler) -> Result<Self> {
        Ok(Self { resolc: revive_path, solc: solc_compiler })
    }

    pub fn new_from_path(revive_path: PathBuf, solc_path: PathBuf) -> Result<Self> {
        let solc = Solc::new(solc_path)?;
        Ok(Self { resolc: revive_path, solc: SolcCompiler::Specific(solc) })
    }

    fn solc(&self, _input: &ResolcVersionedInput) -> Result<Solc> {
        let solc = match &self.solc {
            SolcCompiler::Specific(solc) => solc.clone(),

            #[cfg(feature = "svm-solc")]
            SolcCompiler::AutoDetect => Solc::find_or_install(&_input.solc_version)?,
        };

        Ok(solc)
    }

    pub fn solc_available_versions() -> Vec<Version> {
        let mut ret = vec![];
        let min_max_patch_by_minor_versions =
            vec![(4, 12, 26), (5, 0, 17), (6, 0, 12), (7, 0, 6), (8, 0, 28)];
        for (minor, min_patch, max_patch) in min_max_patch_by_minor_versions {
            for i in min_patch..=max_patch {
                ret.push(Version::new(0, minor, i));
            }
        }

        ret
    }

    pub fn get_version_for_path(path: &Path) -> Result<Version> {
        let mut cmd = Command::new(path);
        cmd.arg("--version").stdin(Stdio::piped()).stderr(Stdio::piped()).stdout(Stdio::piped());
        debug!("Getting Resolc version");
        let output = cmd.output().map_err(map_io_err(path))?;
        trace!(?output);
        let version = version_from_output(output)?;
        debug!(%version);
        Ok(version)
    }

    #[instrument(name = "compile", level = "debug", skip_all)]
    pub fn compile_output<T: Serialize>(
        &self,
        solc: &Solc,
        input: &ResolcInput,
    ) -> Result<Vec<u8>> {
        let mut cmd = self.configure_cmd(solc);
        if !solc.allow_paths.is_empty() {
            cmd.arg("--allow-paths");
            cmd.arg(solc.allow_paths.iter().map(|p| p.display()).join(","));
        }
        if let Some(base_path) = &solc.base_path {
            for path in solc.include_paths.iter().filter(|p| p.as_path() != base_path.as_path()) {
                cmd.arg("--include-path").arg(path);
            }

            cmd.arg("--base-path").arg(base_path);
            cmd.current_dir(base_path);
        }

        let child = if matches!(&input.language, SolcLanguage::Solidity) {
            cmd.arg("--solc");
            cmd.arg(&solc.solc);
            cmd.arg("--standard-json");
            let mut child = cmd.spawn().map_err(map_io_err(&self.resolc))?;
            let mut stdin = io::BufWriter::new(child.stdin.take().unwrap());
            serde_json::to_writer(&mut stdin, &input)?;
            stdin.flush().map_err(map_io_err(&self.resolc))?;
            child
        } else {
            cmd.arg("--yul");
            cmd.arg(format!(
                "{}",
                &input
                    .sources
                    .first_key_value()
                    .map(|k| k.0.to_string_lossy())
                    .ok_or_else(|| SolcError::msg("No Yul sources available"))?
            ));
            cmd.arg("--bin");
            cmd.spawn().map_err(map_io_err(&self.resolc))?
        };

        debug!("Spawned");

        let output = child.wait_with_output().map_err(map_io_err(&self.resolc))?;
        debug!("Finished compiling with standard json with status {:?}", output.status);

        compile_output(output)
    }

    fn configure_cmd(&self, solc: &Solc) -> Command {
        let mut cmd = Command::new(&self.resolc);
        cmd.stdin(Stdio::piped()).stderr(Stdio::piped()).stdout(Stdio::piped());
        cmd.args(&solc.extra_args);
        cmd
    }
}

fn map_io_err(resolc_path: &Path) -> impl FnOnce(std::io::Error) -> SolcError + '_ {
    move |err| SolcError::io(err, resolc_path)
}

fn version_from_output(output: Output) -> Result<Version> {
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .find(|l| l.contains("version"))
            .ok_or_else(|| SolcError::msg("Version not found in resolc output"))?;

        version
            .split_whitespace()
            .find(|s| s.starts_with("0.") || s.starts_with("v0."))
            .and_then(|s| {
                let trimmed = s.trim_start_matches('v').split('+').next().unwrap_or(s);
                Version::from_str(trimmed).ok()
            })
            .ok_or_else(|| SolcError::msg("Unable to retrieve version from resolc output"))
    } else {
        Err(SolcError::solc_output(&output))
    }
}

fn compile_output(output: Output) -> Result<Vec<u8>> {
    // @TODO: Handle YUL output
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(SolcError::solc_output(&output))
    }
}

#[cfg(test)]
mod tests {
    use crate::ProjectBuilder;

    use super::*;
    use semver::Version;
    use std::os::unix::process::ExitStatusExt;
    use which::which;

    fn resolc_instance() -> Resolc {
        Resolc::new(which::which("resolc").unwrap(), SolcCompiler::AutoDetect).unwrap()
    }

    #[test]
    fn test_version_parsing() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"resolc version v0.1.0\n".to_vec(),
            stderr: Vec::new(),
        };
        let version = version_from_output(output);
        assert!(version.is_ok());
        let version = version.unwrap();
        assert_eq!(version.major, 0);
        assert_eq!(version.minor, 1);
        assert_eq!(version.patch, 0);
    }

    #[test]
    fn test_failed_version_parsing() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: b"error\n".to_vec(),
        };
        let version = version_from_output(output);
        assert!(version.is_err());
    }

    #[test]
    fn test_invalid_version_output() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"invalid version format\n".to_vec(),
            stderr: Vec::new(),
        };
        let version = version_from_output(output);
        assert!(version.is_err());
    }

    #[test]
    fn test_compile_output_success() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"test output".to_vec(),
            stderr: Vec::new(),
        };
        let result = compile_output(output);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"test output");
    }

    #[test]
    fn test_compile_output_failure() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: b"error".to_vec(),
        };
        let result = compile_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn test_solc_available_versions_sorted() {
        let versions = Resolc::solc_available_versions();
        let mut sorted = versions.clone();
        sorted.sort();
        assert_eq!(versions, sorted, "Versions should be returned in sorted order");

        for version in versions {
            assert_eq!(version.major, 0, "Major version should be 0");
            assert!(
                version.minor >= 4 && version.minor <= 8,
                "Minor version should be between 4 and 8"
            );
        }
    }

    #[test]
    fn test_resolc_installation_and_compilation() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .with_file(true)
            .with_line_number(true)
            .with_thread_ids(true)
            .try_init();

        let solc = Solc::new(which("solc").unwrap()).unwrap();
        let resolc = Resolc::new(which::which("resolc").unwrap(), SolcCompiler::Specific(solc))
            .expect("Should create Resolc instance from installed binary");

        let project = ProjectBuilder::<Resolc>::new(Default::default())
            .settings(Default::default())
            .build(resolc)
            .unwrap();

        let input = include_str!("../../../../../test-data/resolc/input/compile-input.json");
        let input: ResolcInput = serde_json::from_str(input).expect("Should parse test input JSON");
        let input = ResolcVersionedInput { input, solc_version: Version::new(0, 8, 28) };
        let compilation_result = project.compiler.compile(&input);

        match compilation_result {
            Ok(output) => {
                assert!(
                    !output.errors.iter().any(|err| err.severity.is_error()),
                    "Compilation should not have errors"
                );
            }
            Err(e) => {
                trace!("Error compiling: {:?}", e);
            }
        }
    }

    #[test]
    fn test_compile_with_invalid_utf8() {
        let resolc = resolc_instance();
        let mut cmd = Command::new(&resolc.resolc);
        cmd.arg("--standard-json");
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![0xFF, 0xFF, 0xFF, 0xFF],
            stderr: Vec::new(),
        };
        let bytes = compile_output(output).unwrap();
        let result = String::from_utf8(bytes);
        assert!(result.is_err());
    }
}
