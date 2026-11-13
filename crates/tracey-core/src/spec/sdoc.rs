//! StrictDoc (`.sdoc`) spec backend.
//!
//! Bridges `strictdoc_parser` onto [`SpecDoc`] so the format-agnostic
//! extraction and rendering paths handle `.sdoc` exactly like `.md` / `.typ`.

use std::collections::BTreeMap;
use std::ops::Range;

use marq::{
    DocElement, Heading, ReqDefinition, ReqMetadata, ReqStatus, SourceSpan, parse_rule_id, slugify,
};
use strictdoc_parser::{DocumentChild, Node};

use super::{
    NoConfig, RenderInput, RenderOutput, RenderedSection, SpecBackend, SpecDoc, SpecFormat,
    req_anchor_id,
};

/// Synthetic marker prefix for `.sdoc` requirements.
///
/// `.sdoc` has no `r[...]`-style marker; this is the value `@relation(...)`
/// source markers must agree on for matching.
pub const SDOC_PREFIX: &str = "r";

const INFORMATIVE_ELEMENTS: [&str; 2] = ["TEXT", "TERM"];
const FLOW_ELEMENT: &str = "FLOW";
const STEP_ELEMENT: &str = "STEP";

fn is_rule(node: &Node) -> bool {
    !INFORMATIVE_ELEMENTS.contains(&node.node_type.as_str())
}

/// StrictDoc backend.
#[derive(Default)]
pub struct Sdoc;

#[async_trait::async_trait]
impl SpecBackend for Sdoc {
    type Config = NoConfig;

    fn format(&self) -> SpecFormat {
        SpecFormat::Sdoc
    }
    fn name(&self) -> &'static str {
        "sdoc"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["sdoc"]
    }

    async fn parse(&self, content: &str) -> eyre::Result<SpecDoc> {
        parse(content).await
    }

    /// `.sdoc` has no inline marker syntax; the prefix is fixed.
    fn extract_marker_prefix(&self, _content: &str, _span: SourceSpan) -> Option<String> {
        Some(SDOC_PREFIX.to_owned())
    }

    /// `.sdoc` UIDs carry no version; `tracey bump` rewrites the `VERSION`
    /// field instead (see [`versioned_sdoc_nodes`]).
    fn id_range_in_marker(&self, _marker: &str) -> eyre::Result<Range<usize>> {
        Err(eyre::eyre!(
            "sdoc UIDs carry no version; the version lives in the VERSION field"
        ))
    }

    fn diff_inline(&self, old: &str, new: &str) -> Option<String> {
        normative_diff(old, new, &NormativeFields::fleet())
    }

    async fn render_html(
        &self,
        input: RenderInput<'_>,
        _cfg: &NoConfig,
    ) -> eyre::Result<RenderOutput> {
        let RenderInput {
            root,
            sources,
            badge_for,
            slugs,
            ..
        } = input;

        let mut sections = Vec::with_capacity(sources.len());
        for (idx, src) in sources.iter().enumerate() {
            let doc = strictdoc_parser::parse(src.content).map_err(|e| {
                eyre::eyre!("Failed to parse {} as StrictDoc: {}", src.path.display(), e)
            })?;
            let markup_is_markdown = is_markdown_markup(&doc);
            let source_path = root.join(src.path).display().to_string();

            let mut html = String::new();
            let mut elements = Vec::new();
            let mut state = RenderState {
                html: &mut html,
                badge_for: &badge_for,
                source_path: &source_path,
                slugs: &mut *slugs,
            };
            walk_body(
                &doc.body,
                src.content,
                markup_is_markdown,
                1,
                None,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut elements,
                Some(&mut state),
            )
            .await?;

            sections.push(RenderedSection {
                source_idx: idx,
                html,
                elements,
                head_injections: vec![],
            });
        }
        Ok(RenderOutput { sections })
    }
}

/// Walk `body` in document order, emitting [`Heading`] / [`ReqDefinition`]
/// elements and producing a [`SpecDoc`].
///
/// Per-requirement HTML is the rendered `STATEMENT` (via marq when
/// `OPTIONS: MARKUP: Markdown`, otherwise an escaped `<p>`). The doc-level
/// `html` is left empty — display HTML comes from [`Sdoc::render_html`].
pub(super) async fn parse(content: &str) -> eyre::Result<SpecDoc> {
    let doc = strictdoc_parser::parse(content)
        .map_err(|e| eyre::eyre!("Failed to parse StrictDoc: {}", e))?;
    spec_doc_of(&doc, content).await
}

pub(super) async fn parse_with_flow_steps(
    content: &str,
) -> eyre::Result<(SpecDoc, Vec<(String, Vec<String>)>)> {
    let doc = strictdoc_parser::parse(content)
        .map_err(|e| eyre::eyre!("Failed to parse StrictDoc: {}", e))?;
    let spec_doc = spec_doc_of(&doc, content).await?;
    Ok((spec_doc, flow_steps_of(&doc)))
}

async fn spec_doc_of(doc: &strictdoc_parser::Document, content: &str) -> eyre::Result<SpecDoc> {
    let markup_is_markdown = is_markdown_markup(doc);

    let mut reqs = Vec::new();
    let mut headings = Vec::new();
    let mut elements = Vec::new();
    walk_body(
        &doc.body,
        content,
        markup_is_markdown,
        1,
        None,
        &mut reqs,
        &mut headings,
        &mut elements,
        None,
    )
    .await?;

    Ok(SpecDoc {
        raw_metadata: None,
        metadata_format: None,
        frontmatter: None,
        html: String::new(),
        headings,
        reqs,
        code_samples: vec![],
        elements,
        head_injections: vec![],
        inline_code_spans: vec![],
        source_map: Default::default(),
    })
}

fn is_markdown_markup(doc: &strictdoc_parser::Document) -> bool {
    doc.options
        .get("MARKUP")
        .is_some_and(|v| v.eq_ignore_ascii_case("Markdown"))
}

/// HTML-emission state for the optional render pass of [`walk_body`].
struct RenderState<'a> {
    html: &'a mut String,
    badge_for: &'a super::BadgeFn,
    source_path: &'a str,
    slugs: &'a mut super::SlugAllocator,
}

/// Recursive document-order walk shared by [`parse`] and [`Sdoc::render_html`].
///
/// Always accumulates `reqs` / `headings` / `elements`. When `render` is
/// `Some`, additionally emits HTML and threads heading slugs through the
/// cross-file [`SlugAllocator`](super::SlugAllocator); when `None`, heading
/// ids are the raw [`slugify`] result.
#[allow(clippy::too_many_arguments)]
fn walk_body<'a, 'r: 'a>(
    body: &'a [DocumentChild],
    content: &'a str,
    markup_is_markdown: bool,
    depth: u8,
    inherited_status: Option<ReqStatus>,
    reqs: &'a mut Vec<ReqDefinition>,
    headings: &'a mut Vec<Heading>,
    elements: &'a mut Vec<DocElement>,
    mut render: Option<&'a mut RenderState<'r>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        for child in body {
            match child {
                DocumentChild::Section(s) => {
                    let level = depth.min(6);
                    let base = slugify(&s.title);
                    let id = if let Some(st) = &mut render {
                        let id = st.slugs.alloc(&base);
                        st.html.push_str(&format!(
                            "<h{level} id=\"{id}\">{}</h{level}>\n",
                            html_escape::encode_text(&s.title)
                        ));
                        id
                    } else {
                        base
                    };
                    let h = Heading {
                        title: s.title.clone(),
                        id,
                        level,
                        line: s.span.line as usize,
                    };
                    headings.push(h.clone());
                    elements.push(DocElement::Heading(h));
                    walk_body(
                        &s.children,
                        content,
                        markup_is_markdown,
                        depth.saturating_add(1),
                        inherited_status,
                        reqs,
                        headings,
                        elements,
                        render.as_deref_mut(),
                    )
                    .await?;
                }
                DocumentChild::Node(node) => {
                    let status = node_status(node).or(inherited_status);
                    if !is_rule(node) {
                        // `[TEXT]` prose and `[TERM]` definitions: shown in
                        // the rendered spec, but not requirements.
                        if let Some(st) = &mut render {
                            if let Some(stmt) = node.field_text("STATEMENT") {
                                st.html
                                    .push_str(&render_statement(stmt, markup_is_markdown).await);
                                st.html.push('\n');
                            } else if let Some(definition) = node.field_text("DEFINITION") {
                                st.html.push_str(&format!(
                                    "<p><strong>{}</strong>: {}</p>\n",
                                    html_escape::encode_text(
                                        node.field_text("TITLE").unwrap_or("")
                                    ),
                                    html_escape::encode_text(definition)
                                ));
                            }
                        }
                    } else if let Some(def) =
                        build_req(node, content, markup_is_markdown, status).await?
                    {
                        if let Some(st) = &mut render {
                            st.html.push_str(&(st.badge_for)(&def, st.source_path));
                            st.html.push_str(&def.html);
                            st.html.push_str(super::REQ_CONTAINER_CLOSE);
                            st.html.push('\n');
                        }
                        elements.push(DocElement::Req(def.clone()));
                        reqs.push(def);
                    }
                    // Composite `[[TAG]]` nodes nest further nodes (and
                    // sections); they don't open a heading level themselves.
                    if node.composite {
                        walk_body(
                            &node.children,
                            content,
                            markup_is_markdown,
                            depth,
                            status,
                            reqs,
                            headings,
                            elements,
                            render.as_deref_mut(),
                        )
                        .await?;
                    }
                }
                // Included documents are not followed; list the included
                // file in the `include` glob to load it.
                DocumentChild::DocumentFromFile(_) => {}
            }
        }
        Ok(())
    })
}

fn node_status(node: &Node) -> Option<ReqStatus> {
    let status = node.field_text("STATUS")?.trim();
    if status.eq_ignore_ascii_case("retired") {
        Some(ReqStatus::Removed)
    } else if status.eq_ignore_ascii_case("active") {
        Some(ReqStatus::Stable)
    } else if status.eq_ignore_ascii_case("draft") {
        Some(ReqStatus::Draft)
    } else {
        None
    }
}

fn parse_version(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().filter(|v| *v > 0)
}

/// Build a [`ReqDefinition`] from a rule node: `[REQUIREMENT]`, a
/// custom-grammar element such as `[FUNCTIONAL]`, or a composite node.
///
/// Returns `None` when the node lacks a `UID:` field or the UID does not
/// parse as a tracey rule id (skip + warn). A UID with a `+` version suffix
/// is an error.
async fn build_req(
    req: &Node,
    content: &str,
    markup_is_markdown: bool,
    status: Option<ReqStatus>,
) -> eyre::Result<Option<ReqDefinition>> {
    let Some(uid_field) = req.field("UID") else {
        return Ok(None);
    };
    let uid = uid_field.value.text();
    if uid.contains('+') {
        eyre::bail!(
            "StrictDoc UID '{uid}' contains '+'; an SDoc rule's version comes from its VERSION field"
        );
    }
    let Some(mut rule_id) = parse_rule_id(uid) else {
        eprintln!("Warning: invalid StrictDoc UID '{uid}', skipping requirement");
        return Ok(None);
    };
    if let Some(version) = req.field_text("VERSION") {
        match parse_version(version) {
            Some(v) => rule_id.version = v,
            None => eprintln!(
                "Warning: StrictDoc node '{uid}' has VERSION '{version}', which is not a positive integer; using version 1"
            ),
        }
    }

    let req_span = req.span;
    let raw = content
        .get(req_span.start..req_span.end)
        .unwrap_or("")
        .to_string();

    let html = match req.field_text("STATEMENT") {
        Some(stmt) => render_statement(stmt, markup_is_markdown).await,
        None => String::new(),
    };

    let length = req_span.end.saturating_sub(req_span.start);
    // Use the `UID:` value span as the marker so LSP semantic tokens /
    // diagnostics get a non-zero range to anchor to.
    let uid_span = uid_field.value.span();
    Ok(Some(ReqDefinition {
        id: rule_id,
        anchor_id: req_anchor_id(uid),
        marker_span: SourceSpan {
            offset: uid_span.start,
            length: uid_span.end.saturating_sub(uid_span.start),
        },
        span: SourceSpan {
            offset: req_span.start,
            length,
        },
        line: req_span.line as usize,
        metadata: ReqMetadata {
            status,
            ..ReqMetadata::default()
        },
        raw,
        html,
    }))
}

/// Render a `STATEMENT` value: through marq for `MARKUP: Markdown`
/// documents, otherwise as an escaped `<p>`.
async fn render_statement(stmt: &str, markup_is_markdown: bool) -> String {
    if markup_is_markdown {
        marq::render(stmt, &marq::RenderOptions::default())
            .await
            .map(|d| d.html)
            .unwrap_or_else(|_| wrap_paragraph(stmt))
    } else {
        wrap_paragraph(stmt)
    }
}

fn wrap_paragraph(text: &str) -> String {
    format!("<p>{}</p>", html_escape::encode_text(text))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormativeItem {
    Element,
    Field(String),
    ParentRelations,
    /// The UIDs of a composite node's child nodes, in order.
    Steps,
}

impl NormativeItem {
    /// `element`, `parent-relations` and `steps` name the non-field items;
    /// any other name is a field.
    pub fn parse(name: &str) -> Self {
        match name {
            "element" => Self::Element,
            "parent-relations" => Self::ParentRelations,
            "steps" => Self::Steps,
            field => Self::Field(field.to_owned()),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Element => "element",
            Self::Field(name) => name,
            Self::ParentRelations => "parent-relations",
            Self::Steps => "steps",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormativeFields {
    by_element: BTreeMap<String, Vec<NormativeItem>>,
}

impl Default for NormativeFields {
    fn default() -> Self {
        Self::fleet()
    }
}

impl NormativeFields {
    pub fn fleet() -> Self {
        use NormativeItem::{Element, ParentRelations, Steps};
        let fields = |names: &[&str]| -> Vec<NormativeItem> {
            names
                .iter()
                .map(|n| NormativeItem::Field((*n).to_owned()))
                .collect()
        };
        let requirement = |between: &[&str]| -> Vec<NormativeItem> {
            let mut items = vec![Element];
            items.extend(fields(&["STATUS", "VERIFIED_BY"]));
            items.extend(fields(between));
            items.extend(fields(&["ATTRIBUTES"]));
            items.push(ParentRelations);
            items
        };
        let mut flow = fields(&[
            "STATUS",
            "VERIFIED_BY",
            "ACTOR",
            "MILESTONE",
            "PRECONDITIONS",
            "OUTCOME",
            "ATTRIBUTES",
        ]);
        flow.push(Steps);
        let mut step = fields(&["ACTION", "EXPECTED"]);
        step.push(ParentRelations);

        let by_element = [
            ("TERM", fields(&["STATUS", "VALUES", "DEFINITION"])),
            (
                "FUNCTIONAL",
                requirement(&[
                    "WHERE",
                    "WHILE",
                    "WHEN",
                    "IF",
                    "SYSTEM",
                    "KEYWORD",
                    "RESPONSE",
                    "OTHERWISE",
                    "EXAMPLES",
                ]),
            ),
            (
                "QUALITY",
                requirement(&[
                    "SYSTEM",
                    "KEYWORD",
                    "METRIC",
                    "COMPARATOR",
                    "THRESHOLD",
                    "UNIT",
                    "CONDITION",
                ]),
            ),
            ("CONSTRAINT", requirement(&["KEYWORD", "STATEMENT"])),
            (
                "INTERFACE",
                requirement(&["KEYWORD", "STATEMENT", "CONTRACT"]),
            ),
            (FLOW_ELEMENT, flow),
            (STEP_ELEMENT, step),
        ]
        .into_iter()
        .map(|(element, items)| (element.to_owned(), items))
        .collect();
        Self { by_element }
    }

    /// The Fleet Spec Format's list, with each listed element's items
    /// replaced by the given names (see [`NormativeItem::parse`]).
    pub fn with_overrides<'a>(
        overrides: impl IntoIterator<Item = (&'a str, &'a [String])>,
    ) -> Self {
        let mut fields = Self::fleet();
        for (element, names) in overrides {
            fields.by_element.insert(
                element.to_owned(),
                names.iter().map(|n| NormativeItem::parse(n)).collect(),
            );
        }
        fields
    }

    /// An element missing from the list counts its tag, every field except
    /// UID, VERSION, TITLE and NOTES, its relations and its children.
    fn items_for(&self, node: &Node) -> Vec<NormativeItem> {
        if let Some(items) = self.by_element.get(&node.node_type) {
            return items.clone();
        }
        let mut items = vec![NormativeItem::Element];
        items.extend(
            node.fields
                .iter()
                .filter(|f| !matches!(f.name.as_str(), "UID" | "VERSION" | "TITLE" | "NOTES"))
                .map(|f| NormativeItem::Field(f.name.clone())),
        );
        items.push(NormativeItem::ParentRelations);
        if node.composite {
            items.push(NormativeItem::Steps);
        }
        items
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedSdocNode {
    pub uid: String,
    pub version: u32,
    /// Byte range of the `VERSION` value in the document.
    pub version_span: Range<usize>,
    /// Each normative item's name and value; `None` for an absent field.
    pub normative: Vec<(String, Option<String>)>,
}

/// Every node with a `UID` and a positive `VERSION`, rules and terms alike.
pub fn versioned_sdoc_nodes(
    content: &str,
    fields: &NormativeFields,
) -> eyre::Result<Vec<VersionedSdocNode>> {
    let doc = strictdoc_parser::parse(content)
        .map_err(|e| eyre::eyre!("Failed to parse StrictDoc: {}", e))?;
    let mut out = Vec::new();
    for node in all_nodes(&doc.body) {
        let (Some(uid), Some(version_field)) = (node.field_text("UID"), node.field("VERSION"))
        else {
            continue;
        };
        let Some(version) = parse_version(version_field.value.text()) else {
            continue;
        };
        let span = version_field.value.span();
        out.push(VersionedSdocNode {
            uid: uid.to_owned(),
            version,
            version_span: span.start..span.end,
            normative: normative_values(node, &fields.items_for(node)),
        });
    }
    Ok(out)
}

/// Each `FLOW` node's UID with the UIDs of its `STEP` nodes, in order.
pub fn sdoc_flow_steps(content: &str) -> eyre::Result<Vec<(String, Vec<String>)>> {
    let doc = strictdoc_parser::parse(content)
        .map_err(|e| eyre::eyre!("Failed to parse StrictDoc: {}", e))?;
    Ok(flow_steps_of(&doc))
}

fn flow_steps_of(doc: &strictdoc_parser::Document) -> Vec<(String, Vec<String>)> {
    all_nodes(&doc.body)
        .into_iter()
        .filter(|node| node.node_type == FLOW_ELEMENT)
        .filter_map(|flow| {
            let uid = flow.field_text("UID")?;
            let steps = child_nodes(flow)
                .filter(|child| child.node_type == STEP_ELEMENT)
                .filter_map(|step| step.field_text("UID").map(str::to_owned))
                .collect();
            Some((uid.to_owned(), steps))
        })
        .collect()
}

fn all_nodes(body: &[DocumentChild]) -> Vec<&Node> {
    let mut out = Vec::new();
    for child in body {
        match child {
            DocumentChild::Section(s) => out.extend(all_nodes(&s.children)),
            DocumentChild::Node(node) => {
                out.push(node);
                out.extend(all_nodes(&node.children));
            }
            DocumentChild::DocumentFromFile(_) => {}
        }
    }
    out
}

fn child_nodes(node: &Node) -> impl Iterator<Item = &Node> {
    node.children.iter().filter_map(|child| match child {
        DocumentChild::Node(n) => Some(n),
        _ => None,
    })
}

fn normative_values(node: &Node, items: &[NormativeItem]) -> Vec<(String, Option<String>)> {
    items
        .iter()
        .map(|item| {
            let value = match item {
                NormativeItem::Element => Some(node.node_type.clone()),
                NormativeItem::Field(name) => node.field_text(name).map(str::to_owned),
                NormativeItem::ParentRelations => Some(
                    node.relations
                        .iter()
                        .filter(|r| r.relation_type == "Parent")
                        .map(|r| {
                            format!(
                                "{} ({})",
                                r.target().unwrap_or(""),
                                r.role.as_deref().unwrap_or("no role")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                NormativeItem::Steps => Some(
                    child_nodes(node)
                        .filter_map(|child| child.field_text("UID"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            };
            (item.name().to_owned(), value)
        })
        .collect()
}

fn normative_diff(old: &str, new: &str, fields: &NormativeFields) -> Option<String> {
    let old_doc = parse_single_node(old)?;
    let new_doc = parse_single_node(new)?;
    let old_node = first_node(&old_doc)?;
    let new_node = first_node(&new_doc)?;
    let old_values = normative_values(old_node, &fields.items_for(old_node));
    let new_values = normative_values(new_node, &fields.items_for(new_node));
    let version = |node: &Node| node.field_text("VERSION").unwrap_or("1").trim().to_owned();
    let shown = |value: &Option<String>| match value {
        Some(v) => v.replace('\n', " "),
        None => "(absent)".to_owned(),
    };

    let mut out = format!(
        "Normative fields at VERSION {} and at VERSION {}:\n\n",
        version(old_node),
        version(new_node)
    );
    for (name, value) in &new_values {
        let before = old_values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(None);
        if &before == value {
            out.push_str(&format!("- {name}: {}\n", shown(value)));
        } else {
            out.push_str(&format!(
                "- {name}: ~~{}~~ → {}\n",
                shown(&before),
                shown(value)
            ));
        }
    }
    for (name, value) in &old_values {
        if !new_values.iter().any(|(n, _)| n == name) {
            out.push_str(&format!("- {name}: ~~{}~~\n", shown(value)));
        }
    }
    Some(out)
}

fn parse_single_node(raw: &str) -> Option<strictdoc_parser::Document> {
    strictdoc_parser::parse(&format!("[DOCUMENT]\nTITLE: Diff\n\n{}\n", raw.trim_end())).ok()
}

fn first_node(doc: &strictdoc_parser::Document) -> Option<&Node> {
    doc.body.iter().find_map(|child| match child {
        DocumentChild::Node(n) => Some(n),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_yields_reqs_and_per_req_html() {
        let content = "[DOCUMENT]\nTITLE: T\n\nOPTIONS:\n  MARKUP: Markdown\n\n\
[REQUIREMENT]\nUID: BR-001\nSTATEMENT: The bridge **must** connect.\n";
        let doc = parse(content).await.unwrap();
        assert_eq!(doc.reqs.len(), 1);
        assert_eq!(doc.reqs[0].id.to_string(), "BR-001");
        assert!(doc.reqs[0].html.contains("<strong>"));
        assert_eq!(doc.elements.len(), 1);
    }

    #[tokio::test]
    async fn parse_emits_section_headings_in_order() {
        let content = "[DOCUMENT]\nTITLE: T\n\n\
[[SECTION]]\nTITLE: Outer\n\n\
[REQUIREMENT]\nUID: R-1\nSTATEMENT: a\n\n\
[[/SECTION]]\n";
        let doc = parse(content).await.unwrap();
        assert_eq!(doc.headings.len(), 1);
        assert_eq!(doc.headings[0].title, "Outer");
        assert_eq!(doc.headings[0].level, 1);
        // elements: Heading then Req
        assert!(matches!(doc.elements[0], DocElement::Heading(_)));
        assert!(matches!(doc.elements[1], DocElement::Req(_)));
    }

    #[tokio::test]
    async fn parse_includes_composite_and_custom_nodes_but_not_text() {
        let content = "[DOCUMENT]\nTITLE: T\n\n\
[TEXT]\nSTATEMENT: Intro.\n\n\
[[COMPOSITE_REQUIREMENT]]\nUID: C-1\nSTATEMENT: parent\n\n\
[REQUIREMENT]\nUID: C-1.1\nSTATEMENT: child\n\n\
[[/COMPOSITE_REQUIREMENT]]\n\n\
[FEATURE]\nUID: F-1\nSTATEMENT: feature\n";
        let doc = parse(content).await.unwrap();
        let ids: Vec<String> = doc.reqs.iter().map(|r| r.id.to_string()).collect();
        assert_eq!(ids, ["C-1", "C-1.1", "F-1"]);
    }

    const FLEET_DOC: &str = "[DOCUMENT]\nTITLE: Export\n\n\
[TEXT]\nSTATEMENT: Intro.\n\n\
[TERM]\nUID: glossary.exporter\nVERSION: 1\nTITLE: the exporter\nSTATUS: active\nDEFINITION: Writes exports.\n\n\
[FUNCTIONAL]\nUID: export.file.csv\nVERSION: 1\nTITLE: CSV export\nSTATUS: active\nVERIFIED_BY: test\n\
WHEN: an accountant exports a period\nSYSTEM: the exporter\nKEYWORD: MUST\nRESPONSE: write one CSV file\n\n\
[FUNCTIONAL]\nUID: export.file.name\nVERSION: 2\nTITLE: Export file name\nSTATUS: active\nVERIFIED_BY: test\n\
WHEN: an export file is created\nSYSTEM: the exporter\nKEYWORD: MUST\nRESPONSE: name it by period\n\
NOTES: Accountants keep a year of exports.\nRELATIONS:\n- TYPE: Parent\n  VALUE: export.file.csv\n  ROLE: Refines\n\n\
[QUALITY]\nUID: export.api.latency\nVERSION: 1\nTITLE: Latency\nSTATUS: retired\nVERIFIED_BY: test\n\
SYSTEM: the exporter\nKEYWORD: MUST\nMETRIC: p95\nCOMPARATOR: at most\nTHRESHOLD: 300\nUNIT: ms\nCONDITION: load\n\n\
[CONSTRAINT]\nUID: export.storage.region\nVERSION: 1\nTITLE: EU\nSTATUS: active\nVERIFIED_BY: inspection\n\
KEYWORD: MUST\nSTATEMENT: Files MUST stay in the EU.\n\n\
[INTERFACE]\nUID: export.api.contract\nVERSION: 1\nTITLE: API\nSTATUS: active\nVERIFIED_BY: test\n\
KEYWORD: MUST\nSTATEMENT: The API MUST follow its contract.\nCONTRACT: api/export.yaml\n\n\
[[FLOW]]\nUID: flow.export.month\nVERSION: 1\nTITLE: Export a month\nSTATUS: retired\nVERIFIED_BY: test\n\
ACTOR: accountant\nMILESTONE: 2.0\nOUTCOME: a CSV downloads\n\n\
[STEP]\nUID: flow.export.month.s1\nVERSION: 1\nTITLE: Pick\nACTION: pick a month\nEXPECTED: a file downloads\n\
RELATIONS:\n- TYPE: Parent\n  VALUE: export.file.csv\n  ROLE: Uses\n\n\
[[/FLOW]]\n";

    fn rule(doc: &SpecDoc, base: &str) -> ReqDefinition {
        doc.reqs
            .iter()
            .find(|r| r.id.base == base)
            .unwrap_or_else(|| panic!("{base} is a rule"))
            .clone()
    }

    fn with_field(content: &str, from: &str, to: &str) -> String {
        assert!(content.contains(from), "{from}");
        content.replacen(from, to, 1)
    }

    fn node<'a>(nodes: &'a [VersionedSdocNode], uid: &str) -> &'a VersionedSdocNode {
        nodes
            .iter()
            .find(|n| n.uid == uid)
            .expect("node is versioned")
    }

    #[tokio::test]
    async fn fleet_elements_are_rules_and_terms_and_text_are_not() {
        let doc = parse(FLEET_DOC).await.unwrap();
        let ids: Vec<&str> = doc.reqs.iter().map(|r| r.id.base.as_str()).collect();
        assert_eq!(
            ids,
            [
                "export.file.csv",
                "export.file.name",
                "export.api.latency",
                "export.storage.region",
                "export.api.contract",
                "flow.export.month",
                "flow.export.month.s1",
            ]
        );
    }

    #[tokio::test]
    async fn version_comes_from_the_version_field() {
        let doc = parse(FLEET_DOC).await.unwrap();
        assert_eq!(rule(&doc, "export.file.name").id.version, 2);
        assert_eq!(rule(&doc, "export.file.csv").id.version, 1);
    }

    #[tokio::test]
    async fn uid_with_a_version_suffix_is_an_error_naming_it() {
        let content = with_field(
            FLEET_DOC,
            "UID: export.file.csv\n",
            "UID: export.file.csv+2\n",
        );
        let error = parse(&content).await.unwrap_err().to_string();
        assert!(error.contains("export.file.csv+2"), "{error}");
    }

    #[tokio::test]
    async fn version_that_is_not_a_positive_integer_counts_as_version_1() {
        let content = with_field(
            FLEET_DOC,
            "UID: export.file.name\nVERSION: 2\n",
            "UID: export.file.name\nVERSION: 2.0\n",
        );
        let doc = parse(&content).await.unwrap();
        assert_eq!(rule(&doc, "export.file.name").id.version, 1);
    }

    #[tokio::test]
    async fn retired_rules_are_removed_and_steps_follow_their_flow() {
        let doc = parse(FLEET_DOC).await.unwrap();
        let status = |base| rule(&doc, base).metadata.status;
        assert_eq!(status("export.api.latency"), Some(ReqStatus::Removed));
        assert_eq!(status("flow.export.month.s1"), Some(ReqStatus::Removed));
        assert_eq!(status("export.file.csv"), Some(ReqStatus::Stable));
    }

    #[test]
    fn flows_list_their_steps_in_order() {
        assert_eq!(
            sdoc_flow_steps(FLEET_DOC).unwrap(),
            [(
                "flow.export.month".to_string(),
                vec!["flow.export.month.s1".to_string()]
            )]
        );
    }

    #[test]
    fn versioned_nodes_include_terms_and_locate_their_version() {
        let nodes = versioned_sdoc_nodes(FLEET_DOC, &NormativeFields::fleet()).unwrap();
        let term = node(&nodes, "glossary.exporter");
        assert_eq!(term.version, 1);
        let name = node(&nodes, "export.file.name");
        assert_eq!(&FLEET_DOC[name.version_span.clone()], "2");
    }

    #[test]
    fn title_and_notes_are_not_normative_but_responses_and_relations_are() {
        let fields = NormativeFields::fleet();
        let normative = |content: &str| {
            node(
                &versioned_sdoc_nodes(content, &fields).unwrap(),
                "export.file.name",
            )
            .normative
            .clone()
        };
        let before = normative(FLEET_DOC);

        let retitled = with_field(FLEET_DOC, "TITLE: Export file name", "TITLE: File name");
        let renoted = with_field(FLEET_DOC, "NOTES: Accountants", "NOTES: Finance");
        assert_eq!(normative(&retitled), before);
        assert_eq!(normative(&renoted), before);

        let reworded = with_field(
            FLEET_DOC,
            "RESPONSE: name it by period",
            "RESPONSE: name it",
        );
        let rerolled = with_field(FLEET_DOC, "ROLE: Refines", "ROLE: Uses");
        assert_ne!(normative(&reworded), before);
        assert_ne!(normative(&rerolled), before);
    }

    #[test]
    fn otherwise_and_examples_of_a_functional_rule_are_normative() {
        let fields = NormativeFields::fleet();
        let normative = |content: &str| {
            node(
                &versioned_sdoc_nodes(content, &fields).unwrap(),
                "export.file.name",
            )
            .normative
            .clone()
        };
        let with_both = with_field(
            FLEET_DOC,
            "RESPONSE: name it by period\n",
            "RESPONSE: name it by period\nOTHERWISE: name it export.csv\nEXAMPLES: 2026-03.csv\n",
        );
        let before = normative(&with_both);

        let otherwise = with_field(
            &with_both,
            "OTHERWISE: name it export.csv",
            "OTHERWISE: refuse the export",
        );
        let examples = with_field(&with_both, "EXAMPLES: 2026-03.csv", "EXAMPLES: march.csv");
        assert_ne!(normative(&otherwise), before);
        assert_ne!(normative(&examples), before);
    }

    #[test]
    fn a_flow_step_set_change_is_normative_for_the_flow() {
        let fields = NormativeFields::fleet();
        let flow = |content: &str| {
            node(
                &versioned_sdoc_nodes(content, &fields).unwrap(),
                "flow.export.month",
            )
            .normative
            .clone()
        };
        let renamed_step = with_field(
            FLEET_DOC,
            "UID: flow.export.month.s1",
            "UID: flow.export.month.s2",
        );
        assert_ne!(flow(&renamed_step), flow(FLEET_DOC));
    }

    #[test]
    fn configured_fields_replace_the_default_for_their_element() {
        let only_keyword = ["KEYWORD".to_string()];
        let fields = NormativeFields::with_overrides([("FUNCTIONAL", &only_keyword[..])]);
        let normative = |content: &str| {
            node(
                &versioned_sdoc_nodes(content, &fields).unwrap(),
                "export.file.name",
            )
            .normative
            .clone()
        };
        let reworded = with_field(
            FLEET_DOC,
            "RESPONSE: name it by period",
            "RESPONSE: name it",
        );
        assert_eq!(normative(&reworded), normative(FLEET_DOC));
    }

    #[tokio::test]
    async fn stale_diff_shows_changed_normative_fields_at_both_versions() {
        let old = rule(&parse(FLEET_DOC).await.unwrap(), "export.file.name").raw;
        let changed = with_field(
            &with_field(
                FLEET_DOC,
                "VERSION: 2\nTITLE: Export file name",
                "VERSION: 3\nTITLE: Export file name",
            ),
            "RESPONSE: name it by period",
            "RESPONSE: name it by tenant",
        );
        let new = rule(&parse(&changed).await.unwrap(), "export.file.name").raw;

        let diff = Sdoc.diff_inline(&old, &new).expect("both texts are nodes");
        assert!(diff.contains("VERSION 2 and at VERSION 3"), "{diff}");
        assert!(
            diff.contains("- RESPONSE: ~~name it by period~~ → name it by tenant"),
            "{diff}"
        );
        assert!(diff.contains("- KEYWORD: MUST\n"), "{diff}");
        assert!(!diff.contains("TITLE"), "{diff}");
    }

    #[test]
    fn extract_marker_prefix_is_fixed() {
        let span = SourceSpan {
            offset: 0,
            length: 0,
        };
        assert_eq!(
            Sdoc.extract_marker_prefix("anything", span).as_deref(),
            Some("r")
        );
    }
}
