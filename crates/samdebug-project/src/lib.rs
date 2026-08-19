//! Read-only Microchip Studio project import and deterministic plan generation.

mod importer;
mod model;
mod pipeline;

pub use importer::{import_cproj, initialize_project};
pub use model::{
    ArtifactInfo, ArtifactRequests, ArtifactsReport, BuildPlan, BuildReport, BuildToolPaths,
    CleanReport, ImportResult, ImportWarning, InitReport, MemoryUsage, SourceInput, SourceKind,
    XmlLocation,
};
pub use pipeline::{artifacts, build, clean};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    MicrochipStudioCproj,
}
