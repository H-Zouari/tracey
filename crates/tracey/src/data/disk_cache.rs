use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracey_core::code_units::{CodeUnit, CodeUnitKind};
use tracey_core::{
    ParseWarning, RefVerb, ReqDefinition, ReqReference, RuleId, SourceSpan, SpecFormat, WarningKind,
};

use super::{CachedSourceFile, CachedSpecFile, LspRange, SpecDiagnosticSource, SpecParse};

const DIR: &str = ".tracey_cache";
const FILE: &str = "sources.bin";
const SPEC_FILE: &str = "specs.bin";
const MAGIC: &[u8; 8] = b"TRACEY\0\x01";
const SPEC_MAGIC: &[u8; 8] = b"TRACEYS\x01";
const CACHEDIR_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by tracey.\n\
# For information about cache directory tags see https://bford.info/cachedir/\n";

pub(super) struct ParsedSource {
    pub content_hash: u64,
    pub refs: Vec<ReqReference>,
    pub parse_warnings: Vec<ParseWarning>,
    pub code_units: Vec<CodeUnit>,
}

// The fork's version string does not change between releases, so the executable's size and
// modification time also key the cache: a different build never reads another build's parses.
fn identity() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok());
    let (len, modified) = exe
        .map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(super::file_modified_nanos)
                .unwrap_or(0);
            (meta.len(), modified)
        })
        .unwrap_or((0, 0));
    format!(
        "{} {} {len} {modified}",
        env!("CARGO_PKG_VERSION"),
        option_env!("TRACEY_GIT_COMMIT").unwrap_or("-")
    )
}

pub(super) struct ParsedSpec {
    pub content_hash: u64,
    pub parse: SpecParse,
}

pub(super) fn load(root: &Path) -> HashMap<PathBuf, ParsedSource> {
    std::fs::read(root.join(DIR).join(FILE))
        .ok()
        .and_then(|bytes| decode(&bytes, &identity()))
        .unwrap_or_default()
}

pub(super) fn save(
    root: &Path,
    files: &HashMap<PathBuf, Arc<CachedSourceFile>>,
) -> std::io::Result<()> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    write_if_missing(&dir.join(".gitignore"), b"*\n")?;
    write_if_missing(&dir.join("CACHEDIR.TAG"), CACHEDIR_TAG)?;
    let temporary = dir.join(format!("{FILE}.{}", std::process::id()));
    std::fs::write(&temporary, encode(files, &identity()))?;
    std::fs::rename(&temporary, dir.join(FILE))
}

pub(super) fn load_specs(root: &Path) -> HashMap<PathBuf, ParsedSpec> {
    std::fs::read(root.join(DIR).join(SPEC_FILE))
        .ok()
        .and_then(|bytes| decode_specs(&bytes, &identity()))
        .unwrap_or_default()
}

pub(super) fn save_specs(
    root: &Path,
    files: &HashMap<PathBuf, CachedSpecFile>,
) -> std::io::Result<()> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    write_if_missing(&dir.join(".gitignore"), b"*\n")?;
    write_if_missing(&dir.join("CACHEDIR.TAG"), CACHEDIR_TAG)?;
    let temporary = dir.join(format!("{SPEC_FILE}.{}", std::process::id()));
    std::fs::write(&temporary, encode_specs(files, &identity()))?;
    std::fs::rename(&temporary, dir.join(SPEC_FILE))
}

fn write_if_missing(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, content)
}

fn encode(files: &HashMap<PathBuf, Arc<CachedSourceFile>>, identity: &str) -> Vec<u8> {
    let mut out = Writer(Vec::with_capacity(64 * 1024));
    out.0.extend_from_slice(MAGIC);
    out.str(identity);
    let named: Vec<(&str, &CachedSourceFile)> = files
        .iter()
        .filter_map(|(path, file)| Some((path.to_str()?, &**file)))
        .collect();
    out.usize(named.len());
    for (path, file) in named {
        out.str(path);
        out.u64(file.content_hash);
        out.usize(file.refs.len());
        for reference in &file.refs {
            out.str(&reference.prefix);
            out.u8(verb_code(reference.verb));
            out.rule(&reference.req_id);
            out.usize(reference.line);
            out.span(reference.span);
        }
        out.usize(file.parse_warnings.len());
        for warning in &file.parse_warnings {
            out.usize(warning.line);
            out.span(warning.span);
            match &warning.kind {
                WarningKind::UnknownVerb(verb) => {
                    out.u8(0);
                    out.str(verb);
                }
                WarningKind::MalformedReference => out.u8(1),
            }
        }
        out.usize(file.code_units.len());
        for unit in &file.code_units {
            out.u8(kind_code(unit.kind));
            match &unit.name {
                Some(name) => {
                    out.u8(1);
                    out.str(name);
                }
                None => out.u8(0),
            }
            out.usize(unit.start_line);
            out.usize(unit.end_line);
            out.usize(unit.start_byte);
            out.usize(unit.end_byte);
            out.usize(unit.req_refs.len());
            for rule in &unit.req_refs {
                out.rule(rule);
            }
        }
    }
    out.0
}

fn decode(bytes: &[u8], identity: &str) -> Option<HashMap<PathBuf, ParsedSource>> {
    let mut input = Reader(bytes);
    if input.take(MAGIC.len())? != MAGIC || input.str()? != identity {
        return None;
    }
    let count = input.count()?;
    let mut files = HashMap::with_capacity(count);
    for _ in 0..count {
        let path = PathBuf::from(input.str()?);
        let content_hash = input.u64()?;
        let n = input.count()?;
        let mut refs = Vec::with_capacity(n);
        for _ in 0..n {
            refs.push(ReqReference {
                prefix: input.str()?.to_string(),
                verb: verb_of(input.u8()?)?,
                req_id: input.rule()?,
                file: path.clone(),
                line: input.usize()?,
                span: input.span()?,
            });
        }
        let n = input.count()?;
        let mut parse_warnings = Vec::with_capacity(n);
        for _ in 0..n {
            let line = input.usize()?;
            let span = input.span()?;
            let kind = match input.u8()? {
                0 => WarningKind::UnknownVerb(input.str()?.to_string()),
                1 => WarningKind::MalformedReference,
                _ => return None,
            };
            parse_warnings.push(ParseWarning {
                file: path.clone(),
                line,
                span,
                kind,
            });
        }
        let n = input.count()?;
        let mut code_units = Vec::with_capacity(n);
        for _ in 0..n {
            let kind = kind_of(input.u8()?)?;
            let name = match input.u8()? {
                0 => None,
                1 => Some(input.str()?.to_string()),
                _ => return None,
            };
            let start_line = input.usize()?;
            let end_line = input.usize()?;
            let start_byte = input.usize()?;
            let end_byte = input.usize()?;
            let refs_of_unit = input.count()?;
            let mut req_refs = Vec::with_capacity(refs_of_unit);
            for _ in 0..refs_of_unit {
                req_refs.push(input.rule()?);
            }
            code_units.push(CodeUnit {
                kind,
                name,
                file: path.clone(),
                start_line,
                end_line,
                start_byte,
                end_byte,
                req_refs,
            });
        }
        files.insert(
            path,
            ParsedSource {
                content_hash,
                refs,
                parse_warnings,
                code_units,
            },
        );
    }
    input.0.is_empty().then_some(files)
}

fn encode_specs(files: &HashMap<PathBuf, CachedSpecFile>, identity: &str) -> Vec<u8> {
    let mut out = Writer(Vec::with_capacity(64 * 1024));
    out.0.extend_from_slice(SPEC_MAGIC);
    out.str(identity);
    let named: Vec<(&str, &CachedSpecFile)> = files
        .iter()
        .filter_map(|(path, file)| Some((path.to_str()?, file)))
        .collect();
    out.usize(named.len());
    for (path, file) in named {
        out.str(path);
        out.u64(file.content_hash);
        out.usize(file.parsed.rules.len());
        for rule in &file.parsed.rules {
            let def = &rule.def;
            out.str(&def.id.base);
            out.u64(u64::from(def.id.version));
            out.str(&def.anchor_id);
            out.marq_span(def.marker_span);
            out.marq_span(def.span);
            out.usize(def.line);
            out.option_str(def.metadata.status.map(|status| status.as_str()));
            out.option_str(def.metadata.level.map(|level| level.as_str()));
            out.option_str(def.metadata.since.as_deref());
            out.option_str(def.metadata.until.as_deref());
            out.usize(def.metadata.tags.len());
            for tag in &def.metadata.tags {
                out.str(tag);
            }
            out.str(&def.raw);
            out.str(&def.html);
            out.str(&rule.source_file);
            out.str(rule.format.name());
            out.str(&rule.prefix);
            out.option_usize(rule.column);
            out.option_str(rule.section.as_deref());
            out.option_str(rule.section_title.as_deref());
            out.usize(rule.steps.len());
            for step in &rule.steps {
                out.str(step);
            }
        }
        let diagnostics = &file.parsed.diagnostics;
        out.usize(diagnostics.definitions.len());
        for (rule_id, range) in &diagnostics.definitions {
            out.rule(rule_id);
            out.range(*range);
        }
        out.usize(diagnostics.references.len());
        for (prefix, rule_id, range) in &diagnostics.references {
            out.str(prefix);
            out.rule(rule_id);
            out.range(*range);
        }
    }
    out.0
}

fn decode_specs(bytes: &[u8], identity: &str) -> Option<HashMap<PathBuf, ParsedSpec>> {
    let mut input = Reader(bytes);
    if input.take(SPEC_MAGIC.len())? != SPEC_MAGIC || input.str()? != identity {
        return None;
    }
    let count = input.count()?;
    let mut files = HashMap::with_capacity(count);
    for _ in 0..count {
        let path = PathBuf::from(input.str()?);
        let content_hash = input.u64()?;
        let n = input.count()?;
        let mut rules = Vec::with_capacity(n);
        for _ in 0..n {
            let id = marq::RuleId {
                base: input.str()?.to_string(),
                version: u32::try_from(input.u64()?).ok()?,
            };
            let anchor_id = input.str()?.to_string();
            let marker_span = input.marq_span()?;
            let span = input.marq_span()?;
            let line = input.usize()?;
            let status = match input.option_str()? {
                Some(text) => Some(marq::ReqStatus::parse(text)?),
                None => None,
            };
            let level = match input.option_str()? {
                Some(text) => Some(marq::ReqLevel::parse(text)?),
                None => None,
            };
            let since = input.option_str()?.map(str::to_string);
            let until = input.option_str()?.map(str::to_string);
            let tag_count = input.count()?;
            let mut tags = Vec::with_capacity(tag_count);
            for _ in 0..tag_count {
                tags.push(input.str()?.to_string());
            }
            let raw = input.str()?.to_string();
            let html = input.str()?.to_string();
            let source_file = input.str()?.to_string();
            let format = SpecFormat::from_name(input.str()?)?;
            let prefix = input.str()?.to_string();
            let column = input.option_usize()?;
            let section = input.option_str()?.map(str::to_string);
            let section_title = input.option_str()?.map(str::to_string);
            let step_count = input.count()?;
            let mut steps = Vec::with_capacity(step_count);
            for _ in 0..step_count {
                steps.push(input.str()?.to_string());
            }
            rules.push(std::sync::Arc::new(crate::ExtractedRule {
                def: ReqDefinition {
                    id,
                    anchor_id,
                    marker_span,
                    span,
                    line,
                    metadata: marq::ReqMetadata {
                        status,
                        level,
                        since,
                        until,
                        tags,
                    },
                    raw,
                    html,
                },
                source_file,
                format,
                prefix,
                column,
                section,
                section_title,
                steps,
            }));
        }
        let n = input.count()?;
        let mut definitions = Vec::with_capacity(n);
        for _ in 0..n {
            definitions.push((input.rule()?, input.range()?));
        }
        let n = input.count()?;
        let mut references = Vec::with_capacity(n);
        for _ in 0..n {
            references.push((input.str()?.to_string(), input.rule()?, input.range()?));
        }
        files.insert(
            path,
            ParsedSpec {
                content_hash,
                parse: SpecParse {
                    rules,
                    diagnostics: std::sync::Arc::new(SpecDiagnosticSource {
                        definitions,
                        references,
                    }),
                },
            },
        );
    }
    input.0.is_empty().then_some(files)
}

fn verb_code(verb: RefVerb) -> u8 {
    match verb {
        RefVerb::Define => 0,
        RefVerb::Impl => 1,
        RefVerb::Verify => 2,
        RefVerb::Depends => 3,
        RefVerb::Related => 4,
    }
}

fn verb_of(code: u8) -> Option<RefVerb> {
    Some(match code {
        0 => RefVerb::Define,
        1 => RefVerb::Impl,
        2 => RefVerb::Verify,
        3 => RefVerb::Depends,
        4 => RefVerb::Related,
        _ => return None,
    })
}

fn kind_code(kind: CodeUnitKind) -> u8 {
    match kind {
        CodeUnitKind::Function => 0,
        CodeUnitKind::Struct => 1,
        CodeUnitKind::Enum => 2,
        CodeUnitKind::Trait => 3,
        CodeUnitKind::Impl => 4,
        CodeUnitKind::Module => 5,
        CodeUnitKind::Const => 6,
        CodeUnitKind::Static => 7,
        CodeUnitKind::TypeAlias => 8,
        CodeUnitKind::Macro => 9,
    }
}

fn kind_of(code: u8) -> Option<CodeUnitKind> {
    Some(match code {
        0 => CodeUnitKind::Function,
        1 => CodeUnitKind::Struct,
        2 => CodeUnitKind::Enum,
        3 => CodeUnitKind::Trait,
        4 => CodeUnitKind::Impl,
        5 => CodeUnitKind::Module,
        6 => CodeUnitKind::Const,
        7 => CodeUnitKind::Static,
        8 => CodeUnitKind::TypeAlias,
        9 => CodeUnitKind::Macro,
        _ => return None,
    })
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn str(&mut self, value: &str) {
        self.usize(value.len());
        self.0.extend_from_slice(value.as_bytes());
    }

    fn rule(&mut self, rule: &RuleId) {
        self.str(&rule.base);
        self.u64(u64::from(rule.version));
    }

    fn span(&mut self, span: SourceSpan) {
        self.usize(span.offset);
        self.usize(span.length);
    }

    fn marq_span(&mut self, span: marq::SourceSpan) {
        self.usize(span.offset);
        self.usize(span.length);
    }

    fn option_str(&mut self, value: Option<&str>) {
        match value {
            Some(text) => {
                self.u8(1);
                self.str(text);
            }
            None => self.u8(0),
        }
    }

    fn option_usize(&mut self, value: Option<usize>) {
        match value {
            Some(number) => {
                self.u8(1);
                self.usize(number);
            }
            None => self.u8(0),
        }
    }

    fn range(&mut self, (a, b, c, d): LspRange) {
        for value in [a, b, c, d] {
            self.u64(u64::from(value));
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(length)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn usize(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    fn count(&mut self) -> Option<usize> {
        let count = self.usize()?;
        (count <= self.0.len()).then_some(count)
    }

    fn str(&mut self) -> Option<&'a str> {
        let length = self.usize()?;
        std::str::from_utf8(self.take(length)?).ok()
    }

    fn rule(&mut self) -> Option<RuleId> {
        let base = self.str()?.to_string();
        let version = u32::try_from(self.u64()?).ok()?;
        Some(RuleId { base, version })
    }

    fn span(&mut self) -> Option<SourceSpan> {
        Some(SourceSpan::new(self.usize()?, self.usize()?))
    }

    fn marq_span(&mut self) -> Option<marq::SourceSpan> {
        Some(marq::SourceSpan {
            offset: self.usize()?,
            length: self.usize()?,
        })
    }

    fn option_str(&mut self) -> Option<Option<&'a str>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }

    fn option_usize(&mut self) -> Option<Option<usize>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.usize()?)),
            _ => None,
        }
    }

    fn range(&mut self) -> Option<LspRange> {
        let mut next = || u32::try_from(self.u64()?).ok();
        Some((next()?, next()?, next()?, next()?))
    }
}
