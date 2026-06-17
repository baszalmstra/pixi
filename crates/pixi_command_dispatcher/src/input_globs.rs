//! Compute-engine-driven helper for walking the input glob groups a
//! backend reported.  Each [`InputGlobSet`] is walked via
//! [`InputGlobSetWalkKey`], so two consumers that arrive at the same
//! `(absolute_root, patterns, markers, exclude_hidden)` tuple share a
//! single walk for the engine's lifetime.
//!
//! Backends report inputs as a flat `input_globs: Vec<String>` plus an
//! optional structured `input_glob_sets`. [`fold_input_globs`] normalizes
//! the two into a single `Vec<InputGlobSet>` (the flat list becomes one
//! marker-free, hidden-excluding group) so the rest of the pipeline only
//! ever deals with groups.

use std::{sync::Arc, time::SystemTime};

use pixi_build_types::InputGlobSet;
use pixi_compute_engine::ComputeCtx;
use pixi_compute_fs::{ComputeCtxFsExt, FsError, GlobMTime};
use pixi_glob::GlobSetError;
use pixi_path::{AbsPath, AbsPathBuf};

use crate::keys::{InputGlobSetWalkKey, InputGlobSetWalkSpec};

/// Normalize a backend's `(input_globs, input_glob_sets)` pair into a
/// single list of groups. The flat globs (if any) are folded into one
/// group with default config (no markers, hidden excluded, caller's root).
pub fn fold_input_globs(
    input_globs: Vec<String>,
    input_glob_sets: Option<Vec<InputGlobSet>>,
) -> Vec<InputGlobSet> {
    let mut groups = input_glob_sets.unwrap_or_default();
    if !input_globs.is_empty() {
        groups.push(InputGlobSet {
            patterns: input_globs,
            markers: Vec::new(),
            exclude_hidden: true,
            root: None,
        });
    }
    groups
}

/// Walk every group from the absolute `caller_root`, deduping the resulting
/// (absolute) paths. Groups are independent walks and run concurrently.
/// Overlapping matches across groups (e.g. a flat group folded alongside a
/// structured one) are collapsed.
pub async fn collect_input_files(
    ctx: &mut ComputeCtx,
    groups: &[InputGlobSet],
    caller_root: &AbsPath,
) -> Result<Vec<AbsPathBuf>, Arc<GlobSetError>> {
    let caller_root = caller_root.to_path_buf();
    let per_group = ctx
        .try_compute_join(
            groups.to_vec(),
            async move |sub_ctx: &mut ComputeCtx,
                        group: InputGlobSet|
                        -> Result<Vec<AbsPathBuf>, Arc<GlobSetError>> {
                let key = InputGlobSetWalkKey::from_group(&group, caller_root.as_std_path());
                let paths = sub_ctx.compute(&key).await?;
                Ok(paths
                    .iter()
                    .map(|path| {
                        // The walk root is absolute, so every match is too.
                        AbsPathBuf::new(path.clone())
                            .expect("glob walk of an absolute root yields absolute paths")
                    })
                    .collect())
            },
        )
        .await?;

    let mut all: Vec<AbsPathBuf> = per_group.into_iter().flatten().collect();
    all.sort();
    all.dedup();
    Ok(all)
}

/// Newest mtime observed across a backend's input glob groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputGlobLatestMTime {
    NoMatches,
    MatchesFound {
        modified_at: SystemTime,
        designated_file: AbsPathBuf,
    },
}

/// Compute the newest modification time across all include patterns in the
/// backend input glob groups.
///
/// This is used as a fast stale-cache detector, not as a replacement for exact
/// input discovery. Groups using marker dispatch or exclude patterns are
/// skipped because `glob_mtime` cannot represent those semantics yet; callers
/// still run [`collect_input_files`] afterward when this helper does not prove
/// the cache stale.
pub async fn latest_input_mtime(
    ctx: &mut ComputeCtx,
    groups: &[InputGlobSet],
    caller_root: &AbsPath,
) -> Result<InputGlobLatestMTime, FsError> {
    let queries = groups
        .iter()
        .filter(|group| can_use_glob_mtime(group))
        .flat_map(|group| {
            let spec = InputGlobSetWalkSpec::from_group(group, caller_root.as_std_path());
            spec.patterns
                .into_iter()
                .map(move |pattern| InputGlobMTimeQuery {
                    root: spec.root.clone(),
                    pattern,
                    exclude_hidden: spec.exclude_hidden,
                })
        })
        .collect::<Vec<_>>();

    let per_pattern = ctx
        .try_compute_join(
            queries,
            async move |sub_ctx: &mut ComputeCtx,
                        query: InputGlobMTimeQuery|
                        -> Result<InputGlobLatestMTime, FsError> {
                match sub_ctx.glob_mtime(&query.root, &query.pattern).await? {
                    GlobMTime::NoMatches => Ok(InputGlobLatestMTime::NoMatches),
                    GlobMTime::MatchesFound {
                        designated_file, ..
                    } if query.exclude_hidden
                        && has_hidden_component(&query.root, &designated_file) =>
                    {
                        Ok(InputGlobLatestMTime::NoMatches)
                    }
                    GlobMTime::MatchesFound {
                        modified_at,
                        designated_file,
                    } => Ok(InputGlobLatestMTime::MatchesFound {
                        modified_at,
                        designated_file: AbsPathBuf::new(designated_file).map_err(|err| {
                            FsError::Indexed {
                                message: format!("glob mtime returned invalid path: {err}"),
                            }
                        })?,
                    }),
                }
            },
        )
        .await?;

    Ok(newest_mtime(per_pattern))
}

#[derive(Clone)]
struct InputGlobMTimeQuery {
    root: std::path::PathBuf,
    pattern: String,
    exclude_hidden: bool,
}

fn can_use_glob_mtime(group: &InputGlobSet) -> bool {
    group.markers.is_empty()
        && group
            .patterns
            .iter()
            .all(|pattern| !pattern.starts_with('!'))
}

fn has_hidden_component(root: &std::path::Path, path: &std::path::Path) -> bool {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|component| {
            let std::path::Component::Normal(name) = component else {
                return false;
            };
            name.to_string_lossy().starts_with('.')
        })
}

fn newest_mtime(values: impl IntoIterator<Item = InputGlobLatestMTime>) -> InputGlobLatestMTime {
    values
        .into_iter()
        .filter_map(|value| match value {
            InputGlobLatestMTime::NoMatches => None,
            InputGlobLatestMTime::MatchesFound {
                modified_at,
                designated_file,
            } => Some((modified_at, designated_file)),
        })
        .max_by(|(left_time, left_file), (right_time, right_file)| {
            left_time
                .cmp(right_time)
                .then_with(|| right_file.cmp(left_file))
        })
        .map_or(
            InputGlobLatestMTime::NoMatches,
            |(modified_at, designated_file)| InputGlobLatestMTime::MatchesFound {
                modified_at,
                designated_file,
            },
        )
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use pixi_compute_engine::ComputeEngine;
    use pixi_path::AbsPath;
    use pixi_vfs::IndexedVfs;
    use tempfile::TempDir;

    use super::*;

    fn group(patterns: &[&str], exclude_hidden: bool) -> InputGlobSet {
        InputGlobSet {
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
            markers: Vec::new(),
            exclude_hidden,
            root: None,
        }
    }

    /// Drive `collect_input_files` (needs a `ComputeCtx`) through a throwaway
    /// engine and return the matched paths.
    async fn collect(groups: &[InputGlobSet], root: &AbsPath) -> Vec<AbsPathBuf> {
        ComputeEngine::builder()
            .with_data(Arc::new(IndexedVfs::default()))
            .build()
            .with_ctx(async |ctx| collect_input_files(ctx, groups, root).await)
            .await
            .expect("no cycle")
            .expect("walk succeeds")
    }

    fn set_mtime(path: impl AsRef<std::path::Path>, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    async fn latest(groups: &[InputGlobSet], root: &AbsPath) -> InputGlobLatestMTime {
        ComputeEngine::builder()
            .with_data(Arc::new(IndexedVfs::default()))
            .build()
            .with_ctx(async |ctx| latest_input_mtime(ctx, groups, root).await)
            .await
            .expect("no cycle")
            .expect("glob mtime succeeds")
    }

    #[test]
    fn fold_normalizes_flat_and_structured_into_groups() {
        // Empty in, empty out.
        assert!(fold_input_globs(Vec::new(), None).is_empty());

        // Flat globs become one default group (no markers, hidden excluded,
        // caller's root).
        let folded = fold_input_globs(vec!["a".into(), "b".into()], None);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].patterns, vec!["a".to_string(), "b".to_string()]);
        assert!(folded[0].markers.is_empty());
        assert!(folded[0].exclude_hidden);
        assert!(folded[0].root.is_none());

        // Structured groups pass through unchanged.
        let structured = vec![group(&["x"], false)];
        assert_eq!(
            fold_input_globs(Vec::new(), Some(structured.clone())),
            structured
        );

        // Both: structured first, the folded flat group appended.
        let folded = fold_input_globs(vec!["flat".into()], Some(vec![group(&["x"], true)]));
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].patterns, vec!["x".to_string()]);
        assert_eq!(folded[1].patterns, vec!["flat".to_string()]);
    }

    #[tokio::test]
    async fn latest_mtime_returns_newest_matching_input_file() {
        let tmp = TempDir::new().unwrap();
        let old = tmp.path().join("old.txt");
        let newest = tmp.path().join("src").join("newest.txt");
        fs_err::create_dir(tmp.path().join("src")).unwrap();
        fs_err::write(&old, b"old").unwrap();
        fs_err::write(&newest, b"newest").unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&old, base + Duration::from_secs(1));
        set_mtime(&newest, base + Duration::from_secs(2));
        let root = AbsPath::new(tmp.path()).unwrap();

        assert_eq!(
            latest(&[group(&["**/*.txt"], true)], root).await,
            InputGlobLatestMTime::MatchesFound {
                modified_at: base + Duration::from_secs(2),
                designated_file: AbsPathBuf::new(newest).unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn latest_mtime_skips_hidden_designated_file_when_hidden_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        let visible = tmp.path().join("visible.txt");
        let hidden = tmp.path().join(".pixi").join("hidden.txt");
        fs_err::create_dir(tmp.path().join(".pixi")).unwrap();
        fs_err::write(&visible, b"visible").unwrap();
        fs_err::write(&hidden, b"hidden").unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&visible, base + Duration::from_secs(1));
        set_mtime(&hidden, base + Duration::from_secs(2));
        let root = AbsPath::new(tmp.path()).unwrap();

        assert_eq!(
            latest(&[group(&["**/*.txt"], true)], root).await,
            InputGlobLatestMTime::NoMatches
        );
    }

    #[tokio::test]
    async fn latest_mtime_skips_groups_with_exclude_patterns() {
        let tmp = TempDir::new().unwrap();
        let included = tmp.path().join("included.txt");
        let excluded = tmp.path().join("excluded.txt");
        fs_err::write(&included, b"included").unwrap();
        fs_err::write(&excluded, b"excluded").unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&included, base + Duration::from_secs(1));
        set_mtime(&excluded, base + Duration::from_secs(2));
        let root = AbsPath::new(tmp.path()).unwrap();

        assert_eq!(
            latest(&[group(&["**/*.txt", "!excluded.txt"], true)], root).await,
            InputGlobLatestMTime::NoMatches
        );
    }

    #[tokio::test]
    async fn collect_returns_absolute_matches() {
        let tmp = TempDir::new().unwrap();
        fs_err::write(tmp.path().join("a.txt"), b"x").unwrap();
        let root = AbsPath::new(tmp.path()).unwrap();

        let files = collect(&[group(&["**"], true)], root).await;
        assert!(files.iter().all(|p| p.as_std_path().is_absolute()));
        assert!(files.iter().any(|p| p.as_std_path().ends_with("a.txt")));
    }

    #[tokio::test]
    async fn collect_honors_structured_group_config() {
        // Proves the per-group config (here `exclude_hidden`) is threaded
        // through to the walk rather than ignored.
        let tmp = TempDir::new().unwrap();
        fs_err::write(tmp.path().join(".hidden"), b"x").unwrap();
        let root = AbsPath::new(tmp.path()).unwrap();

        let excluded = collect(&[group(&["**"], true)], root).await;
        assert!(
            !excluded
                .iter()
                .any(|p| p.as_std_path().ends_with(".hidden"))
        );

        let included = collect(&[group(&["**"], false)], root).await;
        assert!(
            included
                .iter()
                .any(|p| p.as_std_path().ends_with(".hidden"))
        );
    }

    #[tokio::test]
    async fn collect_unions_and_dedups_multiple_groups() {
        let tmp = TempDir::new().unwrap();
        fs_err::write(tmp.path().join("a.txt"), b"x").unwrap();
        fs_err::write(tmp.path().join("b.log"), b"x").unwrap();
        let root = AbsPath::new(tmp.path()).unwrap();

        // Two overlapping groups; `a.txt` is matched by both and must appear once.
        let files = collect(&[group(&["**/*.txt"], true), group(&["**"], true)], root).await;
        assert_eq!(
            files
                .iter()
                .filter(|p| p.as_std_path().ends_with("a.txt"))
                .count(),
            1,
            "overlapping matches across groups must be deduped",
        );
        assert!(files.iter().any(|p| p.as_std_path().ends_with("b.log")));
    }
}
