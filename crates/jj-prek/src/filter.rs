//! File filtering logic.
//!
//! Filters file paths based on include/exclude patterns and type tags,
//! following the PREK model. Uses `prek-identify` for real file type
//! identification (shebangs, extensions, binary detection, etc.).

use std::path::{Path, PathBuf};

use prek_identify::TagSet;
use tracing::trace;

use crate::config::FilePattern;

/// Filter files based on include pattern, exclude pattern, and type tags.
///
/// Uses `prek-identify` to determine file types from the actual filesystem,
/// matching the same behavior as prek's filter pipeline.
pub(crate) fn filter_files<'a>(
    files: &'a [PathBuf],
    include: Option<&FilePattern>,
    exclude: Option<&FilePattern>,
    types_or: Option<&TagSet>,
    types: Option<&TagSet>,
    exclude_types: Option<&TagSet>,
) -> Vec<&'a Path> {
    files
        .iter()
        .filter(|path| {
            let path_str = path.to_string_lossy();

            // Apply include filter
            if let Some(pattern) = include {
                if !pattern.is_match(&path_str) {
                    return false;
                }
            }

            // Apply exclude filter
            if let Some(pattern) = exclude {
                if pattern.is_match(&path_str) {
                    return false;
                }
            }

            // If any type filters are specified, identify the file
            if types_or.is_some() || types.is_some() || exclude_types.is_some() {
                let file_tags = match prek_identify::tags_from_path(path) {
                    Ok(tags) => tags,
                    Err(err) => {
                        trace!("Failed to identify {}: {err}", path.display());
                        return false;
                    }
                };

                // types_or: at least one of the specified tags must be present
                if let Some(required) = types_or {
                    if !required.is_empty() && required.is_disjoint(&file_tags) {
                        return false;
                    }
                }

                // types: ALL specified tags must be present
                if let Some(required) = types {
                    if !required.is_empty() && !required.is_subset(&file_tags) {
                        return false;
                    }
                }

                // exclude_types: NONE of the specified tags may be present
                if let Some(excluded) = exclude_types {
                    if !excluded.is_disjoint(&file_tags) {
                        return false;
                    }
                }
            }

            true
        })
        .map(PathBuf::as_path)
        .collect()
}

/// Convert a list of tag name strings into a `TagSet`.
///
/// Unknown tags are silently ignored, matching prek behavior.
pub(crate) fn tags_from_strings(tags: &[String]) -> TagSet {
    TagSet::from_tags(tags.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_by_include() {
        let files = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("tests/test.rs"),
            PathBuf::from("README.md"),
        ];
        let include = FilePattern::Regex(fancy_regex::Regex::new("^src/").unwrap());
        let result = filter_files(&files, Some(&include), None, None, None, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Path::new("src/main.rs"));
    }

    #[test]
    fn filter_by_exclude() {
        let files = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("target/debug/main"),
        ];
        let exclude = FilePattern::Regex(fancy_regex::Regex::new("^target/").unwrap());
        let result = filter_files(&files, None, Some(&exclude), None, None, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Path::new("src/main.rs"));
    }

    #[test]
    fn tags_from_strings_roundtrip() {
        let tags = vec!["rust".to_string(), "text".to_string()];
        let tagset = tags_from_strings(&tags);
        assert!(!tagset.is_empty());
    }
}
