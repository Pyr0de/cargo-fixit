use rustfix::diagnostics::Diagnostic;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub(crate) enum CheckOutput {
    Artifact(Artifact),
    Message(Message),
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub(crate) struct Artifact {
    pub package_id: String,
    pub target: Target,
    pub fresh: bool,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Message {
    pub target: Target,
    pub message: Diagnostic,
    pub package_id: String,
}

#[derive(Deserialize, Hash, PartialEq, Clone, Eq, Debug)]
pub(crate) struct Target {
    kind: Vec<Kind>,
    crate_types: Vec<CrateType>,
    name: String,
    src_path: String,
    edition: String,
    doc: bool,
    doctest: bool,
    test: bool,
}

#[derive(Deserialize, Hash, PartialEq, Clone, Eq, Debug)]
#[serde(rename_all(deserialize = "kebab-case"))]
pub(crate) enum Kind {
    Bin,
    Example,
    Test,
    Bench,
    CustomBuild,
    Lib,
    Rlib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
    #[serde(untagged)]
    Other(String),
}

#[derive(Deserialize, Hash, PartialEq, Clone, Eq, Debug)]
#[serde(rename_all(deserialize = "kebab-case"))]
pub(crate) enum CrateType {
    Bin,
    Lib,
    Rlib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
    #[serde(untagged)]
    Other(String),
}
