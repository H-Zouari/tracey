//! `tracey pre-commit` and `tracey bump` implementation.
//!
//! These commands work directly on the git index (staged files) and do not
//! require the daemon. They detect spec rules whose text was modified without
//! bumping the version number, and can automatically fix them.

use eyre::{Result, WrapErr, bail};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use tracey_core::{
    NormativeFields, SpecFormat, VersionedSdocNode, id_range_in_marker, parse_spec, rewrite_marker,
    versioned_sdoc_nodes,
};

use crate::config::{Config, SpecConfig};

#[derive(Debug)]
pub enum VersionEdit {
    /// The `prefix[id]` marker, whose id gets a `+N` suffix.
    Marker(marq::SourceSpan),
    /// The value of an SDoc node's `VERSION` field.
    SdocVersion(Range<usize>),
}

impl VersionEdit {
    fn start(&self) -> usize {
        match self {
            VersionEdit::Marker(span) => span.offset,
            VersionEdit::SdocVersion(range) => range.start,
        }
    }
}

/// A rule whose text changed in the staged index but whose version was not bumped.
#[derive(Debug)]
pub struct ChangedRule {
    /// Spec file path, relative to project root.
    pub file: PathBuf,
    /// Spec dialect of `file`.
    pub format: SpecFormat,
    /// Rule ID as it appears in the index (version not yet bumped).
    /// Uses `marq::RuleId` since it comes directly from spec parsing.
    pub rule_id: marq::RuleId,
    /// Raw markdown text of the rule before the change (from HEAD).
    pub old_raw: String,
    /// Raw markdown text of the rule after the change (from index).
    pub new_raw: String,
    /// Where to write the bumped version in the **index** content.
    pub edit: VersionEdit,
}

/// Run a git command in the project root and capture stdout.
pub fn git_capture(project_root: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .wrap_err("failed to run git")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    String::from_utf8(out.stdout)
        .wrap_err_with(|| format!("git {} output is not valid UTF-8", args.join(" ")))
}

/// Read a blob from the git object database via `git cat-file blob`.
/// `revision` is `""` for the index (`:path`) or e.g. `"HEAD"` for a commit.
/// Returns `None` if the object doesn't exist (new or deleted file).
/// Returns `Err` if the content is not valid UTF-8.
pub fn git_cat_file(project_root: &Path, revision: &str, path: &str) -> Result<Option<String>> {
    let spec = if revision.is_empty() {
        format!(":{path}")
    } else {
        format!("{revision}:{path}")
    };
    let out = std::process::Command::new("git")
        .args(["cat-file", "blob", &spec])
        .current_dir(project_root)
        .output()
        .wrap_err("failed to run git cat-file")?;

    if !out.status.success() {
        return Ok(None);
    }

    String::from_utf8(out.stdout)
        .map(Some)
        .wrap_err_with(|| format!("content of {spec} is not valid UTF-8"))
}

/// Parse a spec document string and return a map from rule **base** ID → `ReqDefinition`.
async fn parse_spec_rules(
    fmt: SpecFormat,
    content: &str,
) -> Result<HashMap<String, marq::ReqDefinition>> {
    let doc = parse_spec(fmt, content)
        .await
        .map_err(|e| eyre::eyre!("failed to parse spec: {e}"))?;

    Ok(doc
        .reqs
        .into_iter()
        .map(|r| (r.id.base.clone(), r))
        .collect())
}

/// Detect rules that are staged with a text change but no version bump.
///
/// For each staged spec file this function:
/// 1. Reads HEAD and index content via `git cat-file blob`.
/// 2. Parses both with marq.
/// 3. Compares rules that share the same base ID: if `raw` changed but the
///    version number did not increase, the rule is reported as changed.
pub async fn detect_changed_rules(
    project_root: &Path,
    config: &Config,
) -> Result<Vec<ChangedRule>> {
    // Without a HEAD there is nothing to compare against — new repo, first commit.
    let head_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(project_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !head_exists {
        return Ok(vec![]);
    }

    let staged_output = git_capture(
        project_root,
        &["diff-index", "--name-only", "--cached", "HEAD"],
    )?;

    let mut changed_rules = Vec::new();

    for staged_file in staged_output.lines() {
        let staged_file = staged_file.trim();
        if staged_file.is_empty() {
            continue;
        }

        // Only consider files that match a spec include pattern.
        let Some(spec) = config.specs.iter().find(|s| {
            s.include.iter().any(|p| {
                globset::Glob::new(p)
                    .map(|g| g.compile_matcher().is_match(staged_file))
                    .unwrap_or(false)
            })
        }) else {
            continue;
        };

        // Skip files whose extension is not a recognised spec format. A glob
        // like `docs/spec/**/*` could otherwise match images / data files.
        let Some(fmt) = SpecFormat::from_path(Path::new(staged_file)) else {
            continue;
        };

        let old_content = git_cat_file(project_root, "HEAD", staged_file)?;
        let new_content = match git_cat_file(project_root, "", staged_file)? {
            Some(c) => c,
            None => continue, // deleted — nothing to check
        };

        if fmt == SpecFormat::Sdoc {
            let Some(old_content) = old_content else {
                continue;
            };
            changed_rules.extend(changed_sdoc_nodes(
                staged_file,
                &old_content,
                &new_content,
                &normative_fields(spec),
            )?);
            continue;
        }

        let old_rules = match old_content {
            Some(ref c) => parse_spec_rules(fmt, c).await?,
            None => HashMap::new(), // new file
        };
        let new_rules = parse_spec_rules(fmt, &new_content).await?;

        for (base, new_req) in &new_rules {
            let Some(old_req) = old_rules.get(base) else {
                continue; // new rule, no prior version to compare against
            };

            // Text changed but version not bumped → needs a bump.
            if new_req.raw != old_req.raw && new_req.id.version == old_req.id.version {
                changed_rules.push(ChangedRule {
                    file: PathBuf::from(staged_file),
                    format: fmt,
                    rule_id: new_req.id.clone(),
                    old_raw: old_req.raw.clone(),
                    new_raw: new_req.raw.clone(),
                    edit: VersionEdit::Marker(new_req.marker_span),
                });
            }
        }
    }

    Ok(changed_rules)
}

fn normative_fields(spec: &SpecConfig) -> NormativeFields {
    NormativeFields::with_overrides(
        spec.format
            .sdoc
            .normative_fields
            .iter()
            .map(|(element, names)| (element.as_str(), names.as_slice())),
    )
}

fn changed_sdoc_nodes(
    file: &str,
    old_content: &str,
    new_content: &str,
    fields: &NormativeFields,
) -> Result<Vec<ChangedRule>> {
    let old_nodes: HashMap<String, VersionedSdocNode> = versioned_sdoc_nodes(old_content, fields)
        .wrap_err_with(|| format!("{file} at HEAD"))?
        .into_iter()
        .map(|node| (node.uid.clone(), node))
        .collect();
    let describe = |node: &VersionedSdocNode| {
        node.normative
            .iter()
            .map(|(name, value)| format!("{name}: {}", value.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut changed = Vec::new();
    for new_node in versioned_sdoc_nodes(new_content, fields)
        .wrap_err_with(|| format!("{file} in the index"))?
    {
        let Some(old_node) = old_nodes.get(&new_node.uid) else {
            continue;
        };
        if new_node.normative != old_node.normative && new_node.version <= old_node.version {
            changed.push(ChangedRule {
                file: PathBuf::from(file),
                format: SpecFormat::Sdoc,
                rule_id: marq::RuleId {
                    base: new_node.uid.clone(),
                    version: old_node.version,
                },
                old_raw: describe(old_node),
                new_raw: describe(&new_node),
                edit: VersionEdit::SdocVersion(new_node.version_span.clone()),
            });
        }
    }
    Ok(changed)
}

/// Check staged spec changes and exit non-zero if any rule text changed without
/// a version bump. Intended to be called from a git pre-commit hook.
///
/// Prints diagnostics to stderr and returns whether the check passed.
pub async fn pre_commit(project_root: &Path, config: &Config) -> Result<bool> {
    let changes = detect_changed_rules(project_root, config).await?;

    if changes.is_empty() {
        return Ok(true);
    }

    for change in &changes {
        if change.format == SpecFormat::Sdoc {
            eprintln!(
                "error: a normative field of `{}` changed but its VERSION did not increase",
                change.rule_id.base
            );
        } else {
            eprintln!(
                "error: rule `{}` body changed but version was not bumped",
                change.rule_id
            );
        }
        eprintln!("  file: {}", change.file.display());
    }
    eprintln!();
    eprintln!("Hint: run `tracey bump` to automatically bump all changed rules, then re-stage.");
    eprintln!("      Or commit with --no-verify to skip this check.");

    Ok(false)
}

/// Bump the version of every staged rule whose text changed, then re-stage the
/// affected files.
///
/// Edits are applied last-to-first within each file so that earlier byte
/// offsets are not invalidated by preceding edits.
pub async fn bump(project_root: &Path, config: &Config) -> Result<Vec<marq::RuleId>> {
    let changes = detect_changed_rules(project_root, config).await?;

    if changes.is_empty() {
        return Ok(vec![]);
    }

    // Group changes by file.
    let mut by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, change) in changes.iter().enumerate() {
        by_file.entry(change.file.clone()).or_default().push(i);
    }

    let mut bumped_ids = Vec::new();

    for (file, indices) in &by_file {
        let file_str = file.to_string_lossy();
        // All changes in `indices` share the same file, hence the same format.
        let fmt = changes[indices[0]].format;
        let content = git_cat_file(project_root, "", &file_str)?
            .ok_or_else(|| eyre::eyre!("file disappeared from index: {}", file.display()))?;

        let mut bytes = content.into_bytes();

        // Sort indices so we apply edits from last byte offset to first.
        let mut sorted_indices = indices.clone();
        sorted_indices.sort_by_key(|&i| std::cmp::Reverse(changes[i].edit.start()));

        for &idx in &sorted_indices {
            let change = &changes[idx];
            let new_version = change.rule_id.version + 1;

            let (range, replacement) = match &change.edit {
                VersionEdit::Marker(span) => {
                    // Extract the current marker text and rebuild it with the new version.
                    let range = span.offset..span.offset + span.length;
                    let marker_str = std::str::from_utf8(&bytes[range.clone()])
                        .wrap_err("marker is not valid UTF-8")?;

                    // Build the new marker, e.g. `r[auth.login+2]`.
                    let id_range = id_range_in_marker(fmt, marker_str)?;
                    let new_marker =
                        rewrite_marker(marker_str, id_range, &change.rule_id.base, new_version)?;
                    (range, new_marker)
                }
                VersionEdit::SdocVersion(range) => (range.clone(), new_version.to_string()),
            };
            bytes.splice(range, replacement.into_bytes());

            bumped_ids.push(marq::RuleId {
                base: change.rule_id.base.clone(),
                version: new_version,
            });
        }

        // Write the modified content back and re-stage.
        let full_path = project_root.join(file.as_path());
        std::fs::write(&full_path, &bytes)
            .wrap_err_with(|| format!("failed to write {}", full_path.display()))?;

        git_capture(project_root, &["update-index", "--add", "--", &file_str])
            .wrap_err_with(|| format!("failed to re-stage {}", file.display()))?;
    }

    Ok(bumped_ids)
}
