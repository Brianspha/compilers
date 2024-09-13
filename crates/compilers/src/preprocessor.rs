use super::project::Preprocessor;
use crate::{
    flatten::{apply_updates, Updates},
    multi::{MultiCompiler, MultiCompilerInput, MultiCompilerLanguage},
    solc::{SolcCompiler, SolcVersionedInput},
    Compiler, ProjectPathsConfig, Result, SolcError,
};
use alloy_primitives::hex;
use foundry_compilers_artifacts::{
    ast::SourceLocation,
    output_selection::OutputSelection,
    visitor::{Visitor, Walk},
    ContractDefinitionPart, Expression, FunctionCall, FunctionKind, MemberAccess, NewExpression,
    ParameterList, SolcLanguage, Source, SourceUnit, SourceUnitPart, Sources, TypeName,
};
use foundry_compilers_core::utils;
use itertools::Itertools;
use md5::Digest;
use solang_parser::{diagnostics::Diagnostic, helpers::CodeLocation, pt};
use std::{
    collections::BTreeMap,
    fmt::Write,
    path::{Path, PathBuf},
};

/// Removes parts of the contract which do not alter its interface:
///   - Internal functions
///   - External functions bodies
///
/// Preserves all libraries and interfaces.
pub(crate) fn interface_representation(content: &str) -> Result<String, Vec<Diagnostic>> {
    let (source_unit, _) = solang_parser::parse(content, 0)?;
    let mut locs_to_remove = Vec::new();

    for part in source_unit.0 {
        if let pt::SourceUnitPart::ContractDefinition(contract) = part {
            if matches!(contract.ty, pt::ContractTy::Interface(_) | pt::ContractTy::Library(_)) {
                continue;
            }
            for part in contract.parts {
                if let pt::ContractPart::FunctionDefinition(func) = part {
                    let is_exposed = func.ty == pt::FunctionTy::Function
                        && func.attributes.iter().any(|attr| {
                            matches!(
                                attr,
                                pt::FunctionAttribute::Visibility(
                                    pt::Visibility::External(_) | pt::Visibility::Public(_)
                                )
                            )
                        })
                        || matches!(
                            func.ty,
                            pt::FunctionTy::Constructor
                                | pt::FunctionTy::Fallback
                                | pt::FunctionTy::Receive
                        );

                    if !is_exposed {
                        locs_to_remove.push(func.loc);
                    }

                    if let Some(ref body) = func.body {
                        locs_to_remove.push(body.loc());
                    }
                }
            }
        }
    }

    let mut content = content.to_string();
    let mut offset = 0;

    for loc in locs_to_remove {
        let start = loc.start() - offset;
        let end = loc.end() - offset;

        content.replace_range(start..end, "");
        offset += end - start;
    }

    let content = content.replace("\n", "");
    Ok(utils::RE_TWO_OR_MORE_SPACES.replace_all(&content, "").to_string())
}

/// Computes hash of [`interface_representation`] of the source.
pub(crate) fn interface_representation_hash(source: &Source) -> String {
    let Ok(repr) = interface_representation(&source.content) else { return source.content_hash() };
    let mut hasher = md5::Md5::new();
    hasher.update(&repr);
    let result = hasher.finalize();
    hex::encode(result)
}

#[derive(Debug)]
pub struct ItemLocation {
    start: usize,
    end: usize,
}

impl ItemLocation {
    fn try_from_loc(loc: SourceLocation) -> Option<Self> {
        Some(Self { start: loc.start?, end: loc.start? + loc.length? })
    }
}

fn is_test_or_script<L>(path: &Path, paths: &ProjectPathsConfig<L>) -> bool {
    let test_dir = paths.tests.strip_prefix(&paths.root).unwrap_or(&paths.root);
    let script_dir = paths.scripts.strip_prefix(&paths.root).unwrap_or(&paths.root);
    path.starts_with(test_dir) || path.starts_with(script_dir)
}

/// Kind of the bytecode dependency.
#[derive(Debug)]
enum BytecodeDependencyKind {
    /// `type(Contract).creationCode`
    CreationCode,
    /// `new Contract`
    New(ItemLocation, String),
}

/// Represents a single bytecode dependency.
#[derive(Debug)]
struct BytecodeDependency {
    kind: BytecodeDependencyKind,
    loc: ItemLocation,
    referenced_contract: usize,
}

#[derive(Debug)]
struct BytecodeDependencyCollector<'a> {
    source: &'a str,
    dependencies: Vec<BytecodeDependency>,
    total_count: usize,
}

impl BytecodeDependencyCollector<'_> {
    fn new(source: &str) -> BytecodeDependencyCollector<'_> {
        BytecodeDependencyCollector { source, dependencies: Vec::new(), total_count: 0 }
    }
}

impl Visitor for BytecodeDependencyCollector<'_> {
    fn visit_new_expression(&mut self, expr: &NewExpression) {
        if let TypeName::UserDefinedTypeName(_) = &expr.type_name {
            self.total_count += 1;
        }
    }

    fn visit_function_call(&mut self, call: &FunctionCall) {
        let (new_loc, expr) = match &call.expression {
            Expression::NewExpression(expr) => (expr.src, expr),
            Expression::FunctionCallOptions(expr) => {
                if let Expression::NewExpression(new_expr) = &expr.expression {
                    (expr.src, new_expr)
                } else {
                    return;
                }
            }
            _ => return,
        };

        let TypeName::UserDefinedTypeName(type_name) = &expr.type_name else { return };

        let Some(loc) = ItemLocation::try_from_loc(call.src) else { return };
        let Some(name_loc) = ItemLocation::try_from_loc(type_name.src) else { return };
        let Some(new_loc) = ItemLocation::try_from_loc(new_loc) else { return };
        let name = &self.source[name_loc.start..name_loc.end];

        self.dependencies.push(BytecodeDependency {
            kind: BytecodeDependencyKind::New(new_loc, name.to_string()),
            loc,
            referenced_contract: type_name.referenced_declaration as usize,
        });
    }

    fn visit_member_access(&mut self, access: &MemberAccess) {
        if access.member_name != "creationCode" {
            return;
        }
        self.total_count += 1;

        let Expression::FunctionCall(call) = &access.expression else { return };

        let Expression::Identifier(ident) = &call.expression else { return };

        if ident.name != "type" {
            return;
        }

        let Some(Expression::Identifier(ident)) = call.arguments.first() else { return };

        let Some(referenced) = ident.referenced_declaration else { return };

        let Some(loc) = ItemLocation::try_from_loc(access.src) else { return };

        self.dependencies.push(BytecodeDependency {
            kind: BytecodeDependencyKind::CreationCode,
            loc,
            referenced_contract: referenced as usize,
        });
    }
}

fn build_constructor_struct<'a>(
    parameters: &'a ParameterList,
    src: &'a str,
) -> Result<(String, Vec<&'a str>)> {
    let mut s = "struct ConstructorArgs {".to_string();
    let mut param_names = Vec::new();

    for param in &parameters.parameters {
        param_names.push(param.name.as_str());
        if let Some(loc) = ItemLocation::try_from_loc(param.src) {
            let param_def =
                &src[loc.start..loc.end].replace(" memory ", " ").replace(" calldata ", " ");
            write!(s, "{param_def};")?;
        }
    }

    s.push('}');

    Ok((s, param_names))
}

/// Keeps data about a single contract definition.
struct ContractData<'a> {
    /// AST id of the contract.
    ast_id: usize,
    /// Path of the source file.
    path: &'a Path,
    /// Name of the contract
    name: &'a str,
    /// Constructor parameters.
    constructor_params: Option<&'a ParameterList>,
    /// Reference to source code.
    src: &'a str,
    /// Artifact string to pass into cheatcodes.
    artifact: String,
}

impl ContractData<'_> {
    pub fn build_helper(&self) -> Result<Option<String>> {
        let Self { ast_id, path, name, constructor_params, src, .. } = self;

        let Some(params) = constructor_params else { return Ok(None) };
        let (constructor_struct, param_names) = build_constructor_struct(params, src)?;
        let abi_encode = format!(
            "abi.encode({})",
            param_names.iter().map(|name| format!("args.{name}")).join(", ")
        );

        let helper = format!(
            r#"
pragma solidity >=0.4.0;

import "{path}";

abstract contract DeployHelper{ast_id} is {name} {{
    {constructor_struct}
}}

function encodeArgs{ast_id}(DeployHelper{ast_id}.ConstructorArgs memory args) pure returns (bytes memory) {{
    return {abi_encode};
}}
        "#,
            path = path.display(),
        );

        Ok(Some(helper))
    }
}

/// Receives a set of source files along with their ASTs and removes bytecode dependencies from
/// contracts by replacing them with cheatcode invocations.
struct BytecodeDependencyOptimizer<'a> {
    asts: BTreeMap<PathBuf, SourceUnit>,
    paths: &'a ProjectPathsConfig<SolcLanguage>,
    sources: &'a mut Sources,
}

impl BytecodeDependencyOptimizer<'_> {
    fn new<'a>(
        asts: BTreeMap<PathBuf, SourceUnit>,
        paths: &'a ProjectPathsConfig<SolcLanguage>,
        sources: &'a mut Sources,
    ) -> BytecodeDependencyOptimizer<'a> {
        BytecodeDependencyOptimizer { asts, paths, sources }
    }

    fn is_src_file(&self, file: &Path) -> bool {
        let tests = self.paths.tests.strip_prefix(&self.paths.root).unwrap_or(&self.paths.root);
        let scripts = self.paths.scripts.strip_prefix(&self.paths.root).unwrap_or(&self.paths.root);

        !file.starts_with(tests) && !file.starts_with(scripts)
    }

    fn process(self) -> Result<()> {
        let mut updates = Updates::default();

        let contracts = self.collect_contracts();
        let additional_sources = self.create_deploy_helpers(&contracts)?;
        self.remove_bytecode_dependencies(&contracts, &mut updates)?;

        self.sources.extend(additional_sources);

        apply_updates(self.sources, updates);

        Ok(())
    }

    /// Collects a mapping from contract AST id to [ContractData].
    fn collect_contracts(&self) -> BTreeMap<usize, ContractData<'_>> {
        let mut contracts = BTreeMap::new();

        for (path, ast) in &self.asts {
            let src = self.sources.get(path).unwrap().content.as_str();

            if !self.is_src_file(path) {
                continue;
            }

            for node in &ast.nodes {
                if let SourceUnitPart::ContractDefinition(contract) = node {
                    let artifact = format!("{}:{}", path.display(), contract.name);
                    let constructor = contract.nodes.iter().find_map(|node| {
                        let ContractDefinitionPart::FunctionDefinition(func) = node else {
                            return None;
                        };
                        if *func.kind() != FunctionKind::Constructor {
                            return None;
                        }

                        Some(func)
                    });

                    contracts.insert(
                        contract.id,
                        ContractData {
                            artifact,
                            constructor_params: constructor
                                .map(|constructor| &constructor.parameters)
                                .filter(|params| !params.parameters.is_empty()),
                            src,
                            ast_id: contract.id,
                            path,
                            name: &contract.name,
                        },
                    );
                }
            }
        }

        contracts
    }

    /// Creates a helper library used to generate helpers for contract deployment.
    fn create_deploy_helpers(
        &self,
        contracts: &BTreeMap<usize, ContractData<'_>>,
    ) -> Result<Sources> {
        let mut new_sources = Sources::new();
        for (id, contract) in contracts {
            if let Some(code) = contract.build_helper()? {
                let path = format!("foundry-pp/DeployHelper{}.sol", id);
                new_sources.insert(path.into(), Source::new(code));
            }
        }

        Ok(new_sources)
    }

    /// Goes over all source files and replaces bytecode dependencies with cheatcode invocations.
    fn remove_bytecode_dependencies(
        &self,
        contracts: &BTreeMap<usize, ContractData<'_>>,
        updates: &mut Updates,
    ) -> Result<()> {
        for (path, ast) in &self.asts {
            if self.is_src_file(path) {
                continue;
            }
            let src = self.sources.get(path).unwrap().content.as_str();

            if src.is_empty() {
                continue;
            }

            let updates = updates.entry(path.clone()).or_default();
            let mut used_helpers = Vec::new();

            let mut collector = BytecodeDependencyCollector::new(src);
            ast.walk(&mut collector);

            // It is possible to write weird expressions which we won't catch.
            // e.g. (((new Contract)))() is valid syntax
            //
            // We need to ensure that we've collected all dependencies that are in the contract.
            if collector.dependencies.len() != collector.total_count {
                return Err(SolcError::msg(format!(
                    "failed to collect all bytecode dependencies for {}",
                    path.display()
                )));
            }

            let vm_interface_name = format!("VmContractHelper{}", ast.id);
            let vm = format!("{vm_interface_name}(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D)");

            for dep in collector.dependencies {
                let Some(ContractData { artifact, constructor_params, .. }) =
                    contracts.get(&dep.referenced_contract)
                else {
                    continue;
                };
                match dep.kind {
                    BytecodeDependencyKind::CreationCode => {
                        // for creation code we need to just call getCode
                        updates.insert((
                            dep.loc.start,
                            dep.loc.end,
                            format!("{vm}.getCode(\"{artifact}\")"),
                        ));
                    }
                    BytecodeDependencyKind::New(new_loc, name) => {
                        if constructor_params.is_none() {
                            updates.insert((
                                dep.loc.start,
                                dep.loc.end,
                                format!("{name}(payable({vm}.deployCode(\"{artifact}\")))"),
                            ));
                        } else {
                            used_helpers.push(dep.referenced_contract);
                            updates.insert((
                                new_loc.start,
                                new_loc.end,
                                format!("{name}(payable({vm}.deployCode(\"{artifact}\", encodeArgs{id}(DeployHelper{id}.ConstructorArgs", id = dep.referenced_contract),
                            ));
                            updates.insert((dep.loc.end, dep.loc.end, "))))".to_string()));
                        }
                    }
                };
            }
            let helper_imports = used_helpers.into_iter().map(|id| {
                format!(
                    "import {{DeployHelper{id}, encodeArgs{id}}} from \"foundry-pp/DeployHelper{id}.sol\";",
                )
            }).join("\n");
            updates.insert((
                src.len(),
                src.len(),
                format!(
                    r#"
{helper_imports}

interface {vm_interface_name} {{
    function deployCode(string memory _artifact, bytes memory _data) external returns (address);
    function deployCode(string memory _artifact) external returns (address);
    function getCode(string memory _artifact) external returns (bytes memory);
}}"#
                ),
            ));
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct TestOptimizerPreprocessor;

impl Preprocessor<SolcCompiler> for TestOptimizerPreprocessor {
    fn preprocess(
        &self,
        solc: &SolcCompiler,
        mut input: SolcVersionedInput,
        paths: &ProjectPathsConfig<SolcLanguage>,
    ) -> Result<SolcVersionedInput> {
        // Skip if we are not compiling any tests or scripts. Avoids unnecessary solc invocation and
        // AST parsing.
        if input.input.sources.iter().all(|(path, _)| !is_test_or_script(path, paths)) {
            return Ok(input);
        }

        let prev_output_selection = std::mem::replace(
            &mut input.input.settings.output_selection,
            OutputSelection::ast_output_selection(),
        );
        let output = solc.compile(&input)?;

        input.input.settings.output_selection = prev_output_selection;

        if let Some(e) = output.errors.iter().find(|e| e.severity.is_error()) {
            return Err(SolcError::msg(e));
        }

        let asts = output
            .sources
            .into_iter()
            .filter_map(|(path, source)| {
                if !input.input.sources.contains_key(&path) {
                    return None;
                }

                Some((|| {
                    let ast = source.ast.ok_or_else(|| SolcError::msg("missing AST"))?;
                    let ast: SourceUnit = serde_json::from_str(&serde_json::to_string(&ast)?)?;
                    Ok((path, ast))
                })())
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        BytecodeDependencyOptimizer::new(asts, paths, &mut input.input.sources).process()?;

        Ok(input)
    }
}

impl Preprocessor<MultiCompiler> for TestOptimizerPreprocessor {
    fn preprocess(
        &self,
        compiler: &MultiCompiler,
        input: <MultiCompiler as Compiler>::Input,
        paths: &ProjectPathsConfig<MultiCompilerLanguage>,
    ) -> Result<<MultiCompiler as Compiler>::Input> {
        match input {
            MultiCompilerInput::Solc(input) => {
                if let Some(solc) = &compiler.solc {
                    let paths = paths.clone().with_language::<SolcLanguage>();
                    let input = self.preprocess(solc, input, &paths)?;
                    Ok(MultiCompilerInput::Solc(input))
                } else {
                    Ok(MultiCompilerInput::Solc(input))
                }
            }
            MultiCompilerInput::Vyper(input) => Ok(MultiCompilerInput::Vyper(input)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interface_representation() {
        let content = r#"
library Lib {
    function libFn() internal {
        // logic to keep
    }
}
contract A {
    function a() external {}
    function b() public {}
    function c() internal {
        // logic logic logic
    }
    function d() private {}
    function e() external {
        // logic logic logic
    }
}"#;

        let result = interface_representation(content).unwrap();
        assert_eq!(
            result,
            r#"library Lib {function libFn() internal {// logic to keep}}contract A {function a() externalfunction b() publicfunction e() external }"#
        );
    }
}
