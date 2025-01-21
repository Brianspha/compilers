//! Manages compiling of a `Project`
//!
//! The compilation of a project is performed in several steps.
//!
//! First the project's dependency graph [`crate::Graph`] is constructed and all imported
//! dependencies are resolved. The graph holds all the relationships between the files and their
//! versions. From there the appropriate version set is derived
//! [`crate::Graph`] which need to be compiled with different
//! [`crate::compilers::solc::Solc`] versions.
//!
//! At this point we check if we need to compile a source file or whether we can reuse an _existing_
//! `Artifact`. We don't to compile if:
//!     - caching is enabled
//!     - the file is **not** dirty
//!     - the artifact for that file exists
//!
//! This concludes the preprocessing, and we now have either
//!    - only `Source` files that need to be compiled
//!    - only cached `Artifacts`, compilation can be skipped. This is considered an unchanged,
//!      cached project
//!    - Mix of both `Source` and `Artifacts`, only the `Source` files need to be compiled, the
//!      `Artifacts` can be reused.
//!
//! The final step is invoking `Solc` via the standard JSON format.
//!
//! ### Notes on [Import Path Resolution](https://docs.soliditylang.org/en/develop/path-resolution.html#path-resolution)
//!
//! In order to be able to support reproducible builds on all platforms, the Solidity compiler has
//! to abstract away the details of the filesystem where source files are stored. Paths used in
//! imports must work the same way everywhere while the command-line interface must be able to work
//! with platform-specific paths to provide good user experience. This section aims to explain in
//! detail how Solidity reconciles these requirements.
//!
//! The compiler maintains an internal database (virtual filesystem or VFS for short) where each
//! source unit is assigned a unique source unit name which is an opaque and unstructured
//! identifier. When you use the import statement, you specify an import path that references a
//! source unit name. If the compiler does not find any source unit name matching the import path in
//! the VFS, it invokes the callback, which is responsible for obtaining the source code to be
//! placed under that name.
//!
//! This becomes relevant when dealing with resolved imports
//!
//! #### Relative Imports
//!
//! ```solidity
//! import "./math/math.sol";
//! import "contracts/tokens/token.sol";
//! ```
//! In the above `./math/math.sol` and `contracts/tokens/token.sol` are import paths while the
//! source unit names they translate to are `contracts/math/math.sol` and
//! `contracts/tokens/token.sol` respectively.
//!
//! #### Direct Imports
//!
//! An import that does not start with `./` or `../` is a direct import.
//!
//! ```solidity
//! import "/project/lib/util.sol";         // source unit name: /project/lib/util.sol
//! import "lib/util.sol";                  // source unit name: lib/util.sol
//! import "@openzeppelin/address.sol";     // source unit name: @openzeppelin/address.sol
//! import "https://example.com/token.sol"; // source unit name: <https://example.com/token.sol>
//! ```
//!
//! After applying any import remappings the import path simply becomes the source unit name.
//!
//! ##### Import Remapping
//!
//! ```solidity
//! import "github.com/ethereum/dapp-bin/library/math.sol"; // source unit name: dapp-bin/library/math.sol
//! ```
//!
//! If compiled with `solc github.com/ethereum/dapp-bin/=dapp-bin/` the compiler will look for the
//! file in the VFS under `dapp-bin/library/math.sol`. If the file is not available there, the
//! source unit name will be passed to the Host Filesystem Loader, which will then look in
//! `/project/dapp-bin/library/iterable_mapping.sol`
//!
//!
//! ### Caching and Change detection
//!
//! If caching is enabled in the [Project] a cache file will be created upon a successful solc
//! build. The [cache file](crate::cache::CompilerCache) stores metadata for all the files that were
//! provided to solc.
//! For every file the cache file contains a dedicated [cache entry](crate::cache::CacheEntry),
//! which represents the state of the file. A solidity file can contain several contracts, for every
//! contract a separate [artifact](crate::Artifact) is emitted. Therefor the entry also tracks all
//! artifacts emitted by a file. A solidity file can also be compiled with several solc versions.
//!
//! For example in `A(<=0.8.10) imports C(>0.4.0)` and
//! `B(0.8.11) imports C(>0.4.0)`, both `A` and `B` import `C` but there's no solc version that's
//! compatible with `A` and `B`, in which case two sets are compiled: [`A`, `C`] and [`B`, `C`].
//! This is reflected in the cache entry which tracks the file's artifacts by version.
//!
//! The cache makes it possible to detect changes during recompilation, so that only the changed,
//! dirty, files need to be passed to solc. A file will be considered as dirty if:
//!   - the file is new, not included in the existing cache
//!   - the file was modified since the last compiler run, detected by comparing content hashes
//!   - any of the imported files is dirty
//!   - the file's artifacts don't exist, were deleted.
//!
//! Recompiling a project with cache enabled detects all files that meet these criteria and provides
//! solc with only these dirty files instead of the entire source set.

use crate::{
    artifact_output::Artifacts,
    buildinfo::RawBuildInfo,
    cache::ArtifactsCache,
    compile::resolc::{
        output::{AggregatedCompilerOutput, ResolcProjectCompileOutput},
        resolc_artifact_output::{ContractArtifact, ResolcArtifactOutput},
    },
    compilers::{
        resolc::{Resolc, ResolcSettings, ResolcVersionedInput},
        CompilerInput,
    },
    filter::SparseOutputFilter,
    output::Builds,
    report,
    resolver::{parse::SolData, GraphEdges},
    ArtifactOutput, CompilerSettings, Graph, Project, Sources,
};
use foundry_compilers_artifacts::{resolc::ResolcCompilerOutput, SolcLanguage};
use foundry_compilers_core::error::Result;
use rayon::prelude::*;
use semver::Version;
use std::{collections::HashMap, path::PathBuf, time::Instant};

use super::raw_build_info_new;

/// A set of different Solc installations with their version and the sources to be compiled
pub(crate) type VersionedSources<'a, L> =
    HashMap<L, Vec<(Version, Sources, (&'a str, &'a ResolcSettings))>>;

#[derive(Debug)]
pub struct ProjectCompiler<'a> {
    /// Contains the relationship of the source files and their imports
    edges: GraphEdges<SolData>,
    project: &'a Project<Resolc, ResolcArtifactOutput>,
    /// how to compile all the sources
    sources: CompilerSources<'a>,
}

impl<'a> ProjectCompiler<'a> {
    /// Create a new `ProjectCompiler` to bootstrap the compilation process of the project's
    /// sources.
    pub fn new(project: &'a Project<Resolc, ResolcArtifactOutput>) -> Result<Self> {
        Self::with_sources(project, project.paths.read_input_files()?)
    }

    /// Bootstraps the compilation process by resolving the dependency graph of all sources and the
    /// appropriate `Solc` -> `Sources` set as well as the compile mode to use (parallel,
    /// sequential)
    ///
    /// Multiple (`Solc` -> `Sources`) pairs can be compiled in parallel if the `Project` allows
    /// multiple `jobs`, see [`crate::Project::set_solc_jobs()`].
    pub fn with_sources(
        project: &'a Project<Resolc, ResolcArtifactOutput>,
        mut sources: Sources,
    ) -> Result<Self> {
        if let Some(filter) = &project.sparse_output {
            sources.retain(|f, _| filter.is_match(f))
        }
        let graph = Graph::resolve_sources(&project.paths, sources)?;
        let (sources, edges) = graph.into_sources_by_version(project)?;

        // If there are multiple different versions, and we can use multiple jobs we can compile
        // them in parallel.
        let jobs_cnt = || sources.values().map(|v| v.len()).sum::<usize>();
        let sources = CompilerSources {
            jobs: (project.solc_jobs > 1 && jobs_cnt() > 1).then_some(project.solc_jobs),
            sources,
        };

        Ok(Self { edges, project, sources })
    }

    /// Compiles all the sources of the `Project` in the appropriate mode
    ///
    /// If caching is enabled, the sources are filtered and only _dirty_ sources are recompiled.
    ///
    /// The output of the compile process can be a mix of reused artifacts and freshly compiled
    /// `Contract`s
    ///
    /// # Examples
    /// ```no_run
    /// use foundry_compilers::Project;
    ///
    /// let project = Project::builder().build(Default::default())?;
    /// let output = project.compile()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn compile(self) -> Result<ResolcProjectCompileOutput> {
        let slash_paths = self.project.slash_paths;

        // drive the compiler statemachine to completion
        let mut output = self.preprocess()?.compile()?.write_artifacts()?.write_cache()?;

        if slash_paths {
            // ensures we always use `/` paths
            output.slash_paths();
        }

        Ok(output)
    }

    /// Does basic preprocessing
    ///   - sets proper source unit names
    ///   - check cache
    fn preprocess(self) -> Result<PreprocessedState<'a>> {
        trace!("preprocessing");
        let Self { edges, project, mut sources } = self;

        // convert paths on windows to ensure consistency with the `CompilerOutput` `solc` emits,
        // which is unix style `/`
        sources.slash_paths();

        let mut cache = ArtifactsCache::new(project, edges)?;
        // retain and compile only dirty sources and all their imports
        sources.filter(&mut cache);

        Ok(PreprocessedState { sources, cache })
    }
}

/// A series of states that comprise the [`ProjectCompiler::compile()`] state machine
///
/// The main reason is to debug all states individually
#[derive(Debug)]
struct PreprocessedState<'a> {
    /// Contains all the sources to compile.
    sources: CompilerSources<'a>,

    /// Cache that holds `CacheEntry` objects if caching is enabled and the project is recompiled
    cache: ArtifactsCache<'a, ResolcArtifactOutput, Resolc>,
}

impl<'a> PreprocessedState<'a> {
    /// advance to the next state by compiling all sources
    fn compile(self) -> Result<CompiledState<'a>> {
        trace!("compiling");
        let PreprocessedState { sources, mut cache } = self;

        let mut output = sources.compile(&mut cache)?;

        // source paths get stripped before handing them over to solc, so solc never uses absolute
        // paths, instead `--base-path <root dir>` is set. this way any metadata that's derived from
        // data (paths) is relative to the project dir and should be independent of the current OS
        // disk. However internally we still want to keep absolute paths, so we join the
        // contracts again
        output.join_all(cache.project().root());

        Ok(CompiledState { output, cache })
    }
}

/// Represents the state after `solc` was successfully invoked
#[derive(Debug)]
struct CompiledState<'a> {
    output: AggregatedCompilerOutput,
    cache: ArtifactsCache<'a, ResolcArtifactOutput, Resolc>,
}

impl<'a> CompiledState<'a> {
    /// advance to the next state by handling all artifacts
    ///
    /// Writes all output contracts to disk if enabled in the `Project` and if the build was
    /// successful
    #[instrument(skip_all, name = "write-artifacts")]
    fn write_artifacts(self) -> Result<ArtifactsState<'a>> {
        let CompiledState { output, cache } = self;

        let project: &Project<Resolc, ResolcArtifactOutput> = cache.project();
        let ctx = cache.output_ctx();
        // write all artifacts via the handler but only if the build succeeded and project wasn't
        // configured with `no_artifacts == true`
        let compiled_artifacts = if project.no_artifacts {
            project.artifacts_handler().resolc_output_to_artifacts(
                &output.contracts,
                &output.sources,
                ctx,
                &project.paths,
            )
        } else if output.has_error(
            &project.ignored_error_codes,
            &project.ignored_file_paths,
            &project.compiler_severity_filter,
        ) {
            trace!("skip writing cache file due to solc errors: {:?}", output.errors);
            project.artifacts_handler().resolc_output_to_artifacts(
                &output.contracts,
                &output.sources,
                ctx,
                &project.paths,
            )
        } else {
            trace!(
                "handling artifact output for {} contracts and {} sources",
                output.contracts.len(),
                output.sources.len()
            );
            // this emits the artifacts via the project's artifacts handler
            let artifacts = project.artifacts_handler().resolc_on_output(
                &output.contracts,
                &output.sources,
                &project.paths,
                ctx,
            )?;

            // emits all the build infos, if they exist
            output.write_build_infos(project.build_info_path())?;

            artifacts
        };

        Ok(ArtifactsState { output, cache, compiled_artifacts })
    }
}

/// Represents the state after all artifacts were written to disk
#[derive(Debug)]
struct ArtifactsState<'a> {
    output: AggregatedCompilerOutput,
    cache: ArtifactsCache<'a, ResolcArtifactOutput, Resolc>,
    compiled_artifacts: Artifacts<ContractArtifact>,
}

impl<'a> ArtifactsState<'a> {
    /// Writes the cache file
    ///
    /// this concludes the [`Project::compile()`] statemachine
    fn write_cache(self) -> Result<ResolcProjectCompileOutput> {
        let ArtifactsState { output, cache, compiled_artifacts } = self;
        let project = cache.project();
        let ignored_error_codes = project.ignored_error_codes.clone();
        let ignored_file_paths = project.ignored_file_paths.clone();
        let compiler_severity_filter = project.compiler_severity_filter;
        let has_error =
            output.has_error(&ignored_error_codes, &ignored_file_paths, &compiler_severity_filter);
        let skip_write_to_disk = project.no_artifacts || has_error;
        trace!(has_error, project.no_artifacts, skip_write_to_disk, cache_path=?project.cache_path(),"prepare writing cache file");

        let (cached_artifacts, cached_builds) =
            cache.consume(&compiled_artifacts, &output.build_infos, !skip_write_to_disk)?;

        project.artifacts_handler().handle_cached_artifacts(&cached_artifacts)?;

        let builds: Builds<SolcLanguage> = Builds(
            output
                .build_infos
                .iter()
                .map(|build_info| (build_info.id.clone(), build_info.build_context.clone()))
                .chain(cached_builds)
                .map(|(id, context)| (id, context.with_joined_paths(project.paths.root.as_path())))
                .collect(),
        );

        Ok(ResolcProjectCompileOutput {
            compiler_output: output,
            compiled_artifacts,
            cached_artifacts,
            ignored_error_codes,
            ignored_file_paths,
            compiler_severity_filter,
            builds,
        })
    }
}

/// Determines how the `solc <-> sources` pairs are executed.
#[derive(Debug, Clone)]
struct CompilerSources<'a> {
    /// The sources to compile.
    sources: VersionedSources<'a, SolcLanguage>,
    /// The number of jobs to use for parallel compilation.
    jobs: Option<usize>,
}

impl<'a> CompilerSources<'a> {
    /// Converts all `\\` separators to `/`.
    ///
    /// This effectively ensures that `solc` can find imported files like `/src/Cheats.sol` in the
    /// VFS (the `CompilerInput` as json) under `src/Cheats.sol`.
    fn slash_paths(&mut self) {
        #[cfg(windows)]
        {
            use path_slash::PathBufExt;

            self.sources.values_mut().for_each(|versioned_sources| {
                versioned_sources.iter_mut().for_each(|(_, sources, _)| {
                    *sources = std::mem::take(sources)
                        .into_iter()
                        .map(|(path, source)| {
                            (PathBuf::from(path.to_slash_lossy().as_ref()), source)
                        })
                        .collect()
                })
            });
        }
    }

    /// Filters out all sources that don't need to be compiled, see [`ArtifactsCache::filter`]
    fn filter(&mut self, cache: &mut ArtifactsCache<'_, ResolcArtifactOutput, Resolc>) {
        cache.remove_dirty_sources();
        for versioned_sources in self.sources.values_mut() {
            for (version, sources, (profile, _)) in versioned_sources {
                trace!("Filtering {} sources for {}", sources.len(), version);
                cache.filter(sources, version, profile);
                trace!(
                    "Detected {} sources to compile {:?}",
                    sources.dirty().count(),
                    sources.dirty_files().collect::<Vec<_>>()
                );
            }
        }
    }

    /// Compiles all the files with `ReSolc`
    fn compile(
        self,
        cache: &mut ArtifactsCache<'_, ResolcArtifactOutput, Resolc>,
    ) -> Result<AggregatedCompilerOutput> {
        let project = cache.project();
        let graph = cache.graph();

        let jobs_cnt = self.jobs;

        let sparse_output = SparseOutputFilter::new(project.sparse_output.as_deref());

        // Include additional paths collected during graph resolution.
        let mut include_paths = project.paths.include_paths.clone();
        include_paths.extend(graph.include_paths().clone());

        let mut jobs = Vec::new();
        for (language, versioned_sources) in self.sources {
            for (version, sources, (profile, opt_settings)) in versioned_sources {
                let mut opt_settings = opt_settings.clone();
                if sources.is_empty() {
                    // nothing to compile
                    trace!("skip {} for empty sources set", version);
                    continue;
                }

                // depending on the composition of the filtered sources, the output selection can be
                // optimized
                let actually_dirty =
                    sparse_output.sparse_sources(&sources, &mut opt_settings, graph);

                if actually_dirty.is_empty() {
                    // nothing to compile for this particular language, all dirty files are in the
                    // other language set
                    trace!("skip {} run due to empty source set", version);
                    continue;
                }

                trace!("calling {} with {} sources {:?}", version, sources.len(), sources.keys());
                // Revive doesnt seem to support specifying --remappings in standard-json mode
                // But we still need to call .with_remappings here
                let settings = opt_settings
                    .with_base_path(&project.paths.root)
                    .with_allow_paths(&project.paths.allowed_paths)
                    .with_include_paths(&include_paths)
                    .with_remappings(&project.paths.remappings);

                let mut input =
                    ResolcVersionedInput::build(sources, settings, language, version.clone());

                input.strip_prefix(project.paths.root.as_path());

                jobs.push((input, profile, actually_dirty));
            }
        }

        let results: Vec<(ResolcVersionedInput, ResolcCompilerOutput, &str, Vec<PathBuf>)> =
            if let Some(num_jobs) = jobs_cnt {
                compile_parallel(&project.compiler, jobs, num_jobs)
            } else {
                compile_sequential(&project.compiler, jobs)
            }?;

        let mut aggregated: AggregatedCompilerOutput = AggregatedCompilerOutput::default();

        for (input, mut output, profile, actually_dirty) in results {
            let version = input.version();

            // Mark all files as seen by the compiler
            for file in &actually_dirty {
                cache.compiler_seen(file);
            }

            let build_info: RawBuildInfo<SolcLanguage> =
                raw_build_info_new(&input, &output, project.build_info)?;

            output.retain_files(
                actually_dirty
                    .iter()
                    .map(|f| f.strip_prefix(project.paths.root.as_path()).unwrap_or(f)),
            );
            output.join_all(project.paths.root.as_path());

            aggregated.extend(version.clone(), build_info, profile, output);
        }

        Ok(aggregated)
    }
}

type CompilationResult<'a> =
    Result<Vec<(ResolcVersionedInput, ResolcCompilerOutput, &'a str, Vec<PathBuf>)>>;

/// Compiles the input set sequentially and returns a [Vec] of outputs.
fn compile_sequential<'a>(
    compiler: &Resolc,
    jobs: Vec<(ResolcVersionedInput, &'a str, Vec<PathBuf>)>,
) -> CompilationResult<'a> {
    jobs.into_iter()
        .map(|(input, profile, actually_dirty)| {
            let start = Instant::now();
            report::compiler_spawn(
                &input.compiler_name(),
                input.version(),
                actually_dirty.as_slice(),
            );
            let resolc = compiler.resolc(&input)?;
            let output = resolc.compile(&input.input)?;
            report::compiler_success(&input.compiler_name(), input.version(), &start.elapsed());

            Ok((input, output, profile, actually_dirty))
        })
        .collect()
}

/// compiles the input set using `num_jobs` threads
fn compile_parallel<'a>(
    compiler: &Resolc,
    jobs: Vec<(ResolcVersionedInput, &'a str, Vec<PathBuf>)>,
    num_jobs: usize,
) -> CompilationResult<'a> {
    // need to get the currently installed reporter before installing the pool, otherwise each new
    // thread in the pool will get initialized with the default value of the `thread_local!`'s
    // localkey. This way we keep access to the reporter in the rayon pool
    let scoped_report = report::get_default(|reporter| reporter.clone());

    // start a rayon threadpool that will execute all `Solc::compile()` processes
    let pool = rayon::ThreadPoolBuilder::new().num_threads(num_jobs).build().unwrap();

    pool.install(move || {
        jobs.into_par_iter()
            .map(move |(input, profile, actually_dirty)| {
                // set the reporter on this thread
                let _guard = report::set_scoped(&scoped_report);

                let start = Instant::now();
                report::compiler_spawn(
                    &input.compiler_name(),
                    input.version(),
                    actually_dirty.as_slice(),
                );
                let resolc = compiler.resolc(&input)?;
                let result = resolc.compile(&input.input).map(|output| {
                    report::compiler_success(
                        &input.compiler_name(),
                        input.version(),
                        &start.elapsed(),
                    );
                    (input, output, profile, actually_dirty)
                });

                result
            })
            .collect()
    })
}
