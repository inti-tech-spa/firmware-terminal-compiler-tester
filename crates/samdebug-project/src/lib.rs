//! Read-only Microchip Studio project import and deterministic plan generation.

mod importer;
mod model;

pub use importer::{import_cproj, initialize_project};
pub use model::{
    ArtifactRequests, BuildPlan, ImportResult, ImportWarning, InitReport, SourceInput, SourceKind,
    XmlLocation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    MicrochipStudioCproj,
}
