//! tracey-core - Core library for spec coverage analysis
//!
//! This crate provides the building blocks for:
//! - Extracting requirement references from source code (Rust, Swift, TypeScript, and more)
//! - Computing coverage statistics

mod coverage;
mod languages;
mod lexer;
mod markdown;
mod positions;
mod relation;
mod rule_id;
mod sources;
pub mod spec;

#[cfg(feature = "reverse")]
pub mod code_units;
#[cfg(feature = "reverse")]
mod injections;

pub use coverage::CoverageReport;
pub use languages::{arborium_for_ext, devicon_for_ext};
pub use lexer::{ParseWarning, RefVerb, ReqReference, Reqs, SourceSpan, WarningKind};
pub use rule_id::{
    RuleId, RuleIdMatch, classify_reference_for_rule, classify_reference_for_rule_str,
    parse_rule_id,
};
pub use sources::{
    ExtractionResult, MemorySources, PathSources, SUPPORTED_EXTENSIONS, Sources,
    is_supported_extension,
};
pub use spec::{
    BadgeFn, ErasedConfig, NoConfig, NormativeFields, NormativeItem, REQ_ANCHOR_PREFIX,
    RenderInput, RenderOutput, RenderSource, RenderedSection, ReqDefinition, SlugAllocator,
    SpecBackend, SpecConfigs, SpecDoc, SpecFormat, TypstConfig, VersionedSdocNode, diff_inline,
    extract_marker_prefix, id_range_in_marker, is_spec_extension, parse_spec,
    parse_spec_with_flow_steps, parse_weight, render_spec_html, render_spec_inline, req_anchor_id,
    req_anchor_to_id, rewrite_marker, sdoc_flow_steps, versioned_sdoc_nodes,
};

#[cfg(feature = "walk")]
pub use sources::WalkSources;
