use crate::code_units::{CodeUnitKind, typescript_node_kind};
use arborium::tree_sitter::{
    Language, LanguageFn, Node, Parser, Query, QueryCursor, StreamingIterator, Tree,
};
use std::collections::BTreeMap;

pub(crate) struct InjectedLang {
    pub grammar: fn() -> LanguageFn,
    pub node_kind: fn(&str) -> Option<CodeUnitKind>,
}

static TYPESCRIPT: InjectedLang = InjectedLang {
    grammar: arborium_typescript::language,
    node_kind: typescript_node_kind,
};

fn injected_lang(name: &str) -> Option<&'static InjectedLang> {
    match name {
        "javascript" | "typescript" => Some(&TYPESCRIPT),
        _ => None,
    }
}

pub(crate) struct Injection {
    pub tree: Tree,
    pub lang: &'static InjectedLang,
}

pub(crate) fn parse_injections(
    query: &str,
    host_language: &Language,
    host_root: Node,
    source: &str,
) -> Vec<Injection> {
    let Ok(query) = Query::new(host_language, query) else {
        return Vec::new();
    };
    let Some(content_capture) = query.capture_index_for_name("injection.content") else {
        return Vec::new();
    };
    let language_capture = query.capture_index_for_name("injection.language");

    let mut regions: BTreeMap<(usize, usize), (usize, Node, &'static InjectedLang)> =
        BTreeMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, host_root, source.as_bytes());
    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        if !query.general_predicates(pattern).is_empty() {
            continue;
        }
        let settings = query.property_settings(pattern);
        if settings
            .iter()
            .any(|p| matches!(&*p.key, "injection.combined" | "injection.include-children"))
        {
            continue;
        }
        let name = settings
            .iter()
            .find(|p| &*p.key == "injection.language")
            .and_then(|p| p.value.as_deref())
            .or_else(|| {
                let capture = m
                    .captures
                    .iter()
                    .find(|c| Some(c.index) == language_capture)?;
                source.get(capture.node.byte_range())
            });
        let Some(lang) = name.and_then(injected_lang) else {
            continue;
        };
        for capture in m.captures.iter().filter(|c| c.index == content_capture) {
            let node = capture.node;
            if node.child_count() > 0 {
                continue;
            }
            let key = (node.start_byte(), node.end_byte());
            if regions.get(&key).is_none_or(|(seen, ..)| pattern >= *seen) {
                regions.insert(key, (pattern, node, lang));
            }
        }
    }

    regions
        .into_values()
        .filter_map(|(_, node, lang)| {
            let mut parser = Parser::new();
            parser.set_language(&(lang.grammar)().into()).ok()?;
            parser.set_included_ranges(&[node.range()]).ok()?;
            let tree = parser.parse(source, None)?;
            Some(Injection { tree, lang })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svelte_injections(query: &str, source: &str) -> Vec<Injection> {
        let language: Language = arborium_svelte::language().into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .expect("Svelte grammar loads");
        let tree = parser.parse(source, None).expect("Svelte source parses");
        parse_injections(query, &language, tree.root_node(), source)
    }

    #[test]
    fn combined_pattern_is_skipped() {
        let source = "<script>\n  let count = 0;\n</script>\n";
        let fixed = r#"((script_element (raw_text) @injection.content)
            (#set! injection.language "javascript"))"#;
        let combined = r#"((script_element (raw_text) @injection.content)
            (#set! injection.language "javascript")
            (#set! injection.combined))"#;
        assert_eq!(svelte_injections(fixed, source).len(), 1);
        assert_eq!(svelte_injections(combined, source).len(), 0);
    }

    #[test]
    fn language_name_is_read_from_the_captured_text() {
        let query = r#"(script_element
            (start_tag (attribute (quoted_attribute_value (attribute_value) @injection.language)))
            (raw_text) @injection.content)"#;
        let typescript = "<script lang=\"typescript\">\n  let count = 0;\n</script>\n";
        let unknown = "<script lang=\"coffee\">\n  count = 0\n</script>\n";
        assert_eq!(svelte_injections(query, typescript).len(), 1);
        assert_eq!(svelte_injections(query, unknown).len(), 0);
    }

    #[test]
    fn svelte_script_with_lang_ts_is_one_region() {
        let source = "<script lang=\"ts\">\n  let count: number = 0;\n</script>\n";
        let injections = svelte_injections(arborium_svelte::INJECTIONS_QUERY, source);
        assert_eq!(injections.len(), 1);
    }
}
