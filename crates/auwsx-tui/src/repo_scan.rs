//! Background git-repo discovery for the New-project form, mirroring wsx's
//! `repo_scan.rs` + fuzzy completion (`/Users/eliot/ws-ps/wsx/crates/wsx/src/{repo_scan,ui/input}.rs`).
//!
//! A blocking walk from `$HOME` (bounded depth, skipping heavy dirs) collects
//! the absolute path of every directory containing a `.git`. The walk stops
//! descending once it finds a repo, so nested worktrees/submodules don't explode
//! the result set. Results are fuzzy-filtered against the field text.
//!
//! Zero new deps: `$HOME` comes from `std::env`, the walk uses `std::fs`.

use std::path::{Path, PathBuf};

/// Directory names never worth descending into during a repo scan.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "Library",
    "Applications",
    ".Trash",
    ".cargo",
    ".rustup",
];

/// Max directory depth below `$HOME` to descend.
const MAX_SCAN_DEPTH: usize = 8;

/// Walk `$HOME` and return every git repo root found, as display paths
/// (`~/rel/path`). Blocking; intended to run on a dedicated thread.
pub fn scan_git_repos() -> Vec<String> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_for_git(&home, &home, 0, &mut out);
    out.sort();
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Recursive walk. Records `dir` and stops descending when it holds a `.git`.
fn walk_for_git(dir: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // unreadable dir (perms, races): skip, don't fail the whole scan
    };

    let mut has_git = false;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name == ".git" {
            // Only a `.git` DIRECTORY marks a real repo root. A `.git` FILE is a
            // worktree/submodule pointer — not a repo root, so it's ignored.
            if is_dir {
                has_git = true;
            }
            continue;
        }
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if is_dir {
            subdirs.push(entry.path());
        }
    }

    if has_git {
        out.push(format_repo_path(dir, home));
        return; // don't descend into a repo
    }
    for sub in subdirs {
        walk_for_git(&sub, home, depth + 1, out);
    }
}

/// `~/rel/path` for paths under `$HOME`, otherwise the absolute path.
fn format_repo_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Fuzzy subsequence score: every `query` char must appear in order in
/// `target`. Consecutive matches and a prefix match earn bonuses. `None` when
/// `query` is not a subsequence. Empty query scores 0 (matches everything).
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = target.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut consecutive = 0i32;
    for (ti, &tc) in t.iter().enumerate() {
        if qi < q.len() && tc == q[qi] {
            consecutive += 1;
            score += 1 + consecutive; // base + run-length bonus
            if ti == 0 {
                score += 4; // prefix bonus
            }
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    (qi == q.len()).then_some(score)
}

/// Top fuzzy matches for `query` over `repos`, best first, capped at `limit`.
/// Empty query returns the first `limit` repos in their (sorted) order.
pub fn filter_repos(query: &str, repos: &[String], limit: usize) -> Vec<String> {
    let trimmed = query.trim();
    // A path-like query (the user is typing an explicit path) suppresses
    // fuzzy suggestions — they know where they're going.
    if trimmed.starts_with('/') || trimmed.starts_with("~/") {
        return Vec::new();
    }
    let mut scored: Vec<(i32, &str)> = repos
        .iter()
        .filter_map(|r| {
            let rel = r.strip_prefix("~/").unwrap_or(r);
            fuzzy_score(trimmed, rel).map(|s| (s, r.as_str()))
        })
        .collect();
    // Higher score first; ties broken by shorter, then lexicographic path.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, r)| r.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fuzzy_score: subsequence contract -------------------------------

    #[test]
    fn given_empty_query_when_scored_then_some_zero() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn given_subsequence_query_when_scored_then_some() {
        assert!(fuzzy_score("abc", "axbxc").is_some());
    }

    #[test]
    fn given_non_subsequence_query_when_scored_then_none() {
        assert_eq!(fuzzy_score("abc", "acb"), None);
    }

    #[test]
    fn given_chars_not_present_when_scored_then_none() {
        assert_eq!(fuzzy_score("xyz", "abc"), None);
    }

    #[test]
    fn given_query_longer_than_target_when_scored_then_none() {
        assert_eq!(fuzzy_score("abcd", "abc"), None);
    }

    #[test]
    fn given_single_char_query_when_present_then_some() {
        assert!(fuzzy_score("a", "xyzaq").is_some());
    }

    #[test]
    fn given_query_equal_to_target_when_scored_then_some() {
        assert!(fuzzy_score("abc", "abc").is_some());
    }

    #[test]
    fn given_nonempty_query_when_target_empty_then_none() {
        assert_eq!(fuzzy_score("a", ""), None);
    }

    #[test]
    fn given_repeated_query_chars_when_target_has_spread_then_some() {
        assert!(fuzzy_score("aa", "aba").is_some()); // a_a
    }

    #[test]
    fn given_repeated_query_chars_when_target_has_only_one_then_none() {
        assert!(fuzzy_score("aa", "abc").is_none());
    }

    #[test]
    fn given_uppercase_query_when_scored_against_lowercase_then_some() {
        assert!(fuzzy_score("ABC", "abc").is_some());
    }

    #[test]
    fn given_lowercase_query_when_scored_against_uppercase_then_some() {
        assert!(fuzzy_score("abc", "ABC").is_some());
    }

    #[test]
    fn given_prefix_vs_midstring_match_when_scored_then_prefix_higher() {
        let prefix = fuzzy_score("foo", "foobar").unwrap();
        let mid = fuzzy_score("foo", "xfoobar").unwrap();
        assert!(prefix > mid);
    }

    #[test]
    fn given_consecutive_vs_scattered_match_when_scored_then_consecutive_higher() {
        let consecutive = fuzzy_score("abc", "abcxyz").unwrap();
        let scattered = fuzzy_score("abc", "axbxcx").unwrap();
        assert!(consecutive > scattered);
    }

    // --- filter_repos: ordering, cap, path suppression -------------------

    #[test]
    fn given_empty_query_when_filtered_then_returns_first_limit_in_order() {
        let repos = vec!["~/a".to_string(), "~/b".to_string(), "~/c".to_string()];
        assert_eq!(
            filter_repos("", &repos, 2),
            vec!["~/a".to_string(), "~/b".to_string()]
        );
    }

    #[test]
    fn given_whitespace_query_when_filtered_then_returns_first_limit_in_order() {
        let repos = vec!["~/a".to_string(), "~/b".to_string()];
        assert_eq!(filter_repos("   ", &repos, 1), vec!["~/a".to_string()]);
    }

    #[test]
    fn given_slash_prefixed_query_when_filtered_then_empty() {
        let repos = vec!["~/foo".to_string()];
        assert!(filter_repos("/foo", &repos, 8).is_empty());
    }

    #[test]
    fn given_tilde_slash_prefixed_query_when_filtered_then_empty() {
        let repos = vec!["~/foo".to_string()];
        assert!(filter_repos("~/foo", &repos, 8).is_empty());
    }

    #[test]
    fn given_path_query_with_surrounding_whitespace_when_filtered_then_empty() {
        let repos = vec!["~/foo".to_string()];
        assert!(filter_repos("  /foo  ", &repos, 8).is_empty());
    }

    #[test]
    fn given_bare_tilde_query_when_repo_has_tilde_char_then_not_short_circuited() {
        // "~" is NOT path-like (only "/" or "~/" are). It must go through
        // scoring; against "~/a~b" (stripped to "a~b") it subsequence-matches.
        let repos = vec!["~/a~b".to_string()];
        assert!(!filter_repos("~", &repos, 5).is_empty());
    }

    #[test]
    fn given_nonpath_query_with_surrounding_whitespace_when_filtered_then_still_matches() {
        let repos = vec!["~/alpha".to_string()];
        assert_eq!(filter_repos("  alph  ", &repos, 5), vec!["~/alpha".to_string()]);
    }

    #[test]
    fn given_query_matching_nothing_when_filtered_then_empty() {
        let repos = vec!["~/alpha".to_string(), "~/beta".to_string()];
        assert!(filter_repos("zzzz", &repos, 8).is_empty());
    }

    #[test]
    fn given_matching_query_when_filtered_then_contains_match() {
        let repos = vec!["~/alpha".to_string(), "~/beta".to_string()];
        assert_eq!(filter_repos("alph", &repos, 8), vec!["~/alpha".to_string()]);
    }

    #[test]
    fn given_more_matches_than_limit_when_filtered_then_capped() {
        let repos = vec![
            "~/foo1".to_string(),
            "~/foo2".to_string(),
            "~/foo3".to_string(),
        ];
        assert_eq!(filter_repos("foo", &repos, 2).len(), 2);
    }

    #[test]
    fn given_limit_zero_when_filtered_then_empty() {
        assert!(filter_repos("foo", &["~/foo".to_string()], 0).is_empty());
    }

    #[test]
    fn given_empty_repos_slice_when_filtered_then_empty() {
        assert!(filter_repos("foo", &[], 5).is_empty());
    }

    #[test]
    fn given_leading_tilde_slash_when_filtered_then_stripped_before_scoring() {
        let repos = vec!["~/proj".to_string()];
        assert_eq!(filter_repos("proj", &repos, 8), vec!["~/proj".to_string()]);
    }

    #[test]
    fn given_stronger_match_when_filtered_then_ranked_first() {
        let repos = vec!["~/xbar".to_string(), "~/bar".to_string()];
        assert_eq!(filter_repos("bar", &repos, 1), vec!["~/bar".to_string()]);
    }

    // --- format_repo_path ------------------------------------------------

    #[test]
    fn given_path_under_home_when_formatted_then_tilde_relative() {
        assert_eq!(
            format_repo_path(Path::new("/home/u/projects/foo"), Path::new("/home/u")),
            "~/projects/foo".to_string()
        );
    }

    #[test]
    fn given_deeply_nested_path_under_home_when_formatted_then_tilde_relative() {
        assert_eq!(
            format_repo_path(Path::new("/home/u/a/b/c"), Path::new("/home/u")),
            "~/a/b/c".to_string()
        );
    }

    #[test]
    fn given_path_equal_home_when_formatted_then_tilde() {
        assert_eq!(
            format_repo_path(Path::new("/home/u"), Path::new("/home/u")),
            "~".to_string()
        );
    }

    #[test]
    fn given_path_not_under_home_when_formatted_then_absolute() {
        assert_eq!(
            format_repo_path(Path::new("/etc/x"), Path::new("/home/u")),
            "/etc/x".to_string()
        );
    }

    #[test]
    fn given_path_sharing_string_prefix_with_home_but_outside_when_formatted_then_absolute() {
        // "/home/user2" string-starts-with "/home/u" but is NOT under it — a
        // component-wise check must keep it absolute, not yield "~/ser2/x".
        assert_eq!(
            format_repo_path(Path::new("/home/user2/x"), Path::new("/home/u")),
            "/home/user2/x".to_string()
        );
    }

    // --- walk_for_git: recursive scan over real temp trees ---------------

    use tempfile::tempdir;

    #[test]
    fn given_dir_with_dot_git_when_walk_then_repo_recorded() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.contains(&"~/a".to_string()));
    }

    #[test]
    fn given_dot_git_is_file_not_dir_when_walk_then_not_recorded() {
        // A `.git` FILE (worktree/submodule pointer) is not a repo root — only a
        // `.git` directory counts.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/.git"), b"gitdir: ../real/.git").unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.is_empty(), ".git file must not register as a repo; got: {out:?}");
    }

    #[test]
    fn given_root_is_repo_when_walk_then_tilde_recorded() {
        // dir == home and the root itself directly contains .git → "~".
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert_eq!(out, vec!["~".to_string()]);
    }

    #[test]
    fn given_nested_repo_inside_repo_when_walk_then_inner_not_recorded() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/.git")).unwrap();
        std::fs::create_dir_all(root.join("a/sub/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.contains(&"~/a".to_string()));
        assert!(!out.iter().any(|s| s.contains("sub")));
    }

    #[test]
    fn given_repo_inside_skip_dir_when_walk_then_not_recorded() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("node_modules/pkg/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(!out.iter().any(|s| s.contains("node_modules") || s.contains("pkg")));
    }

    #[test]
    fn given_all_skip_dir_names_when_walk_then_none_descended() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for skip in SKIP_DIRS {
            std::fs::create_dir_all(root.join(skip).join("repo/.git")).unwrap();
        }
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.is_empty(), "all skip-dir repos must be pruned; got: {out:?}");
    }

    #[test]
    fn given_repo_inside_dot_dir_when_walk_then_not_recorded() {
        // .config is a dot-dir (distinct from the .git marker) → pruned.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".config/thing/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.is_empty(), "dot-dirs must be pruned; got: {out:?}");
    }

    #[test]
    fn given_repo_beyond_max_depth_when_walk_then_not_recorded() {
        // d9 sits 9 levels below root (> MAX_SCAN_DEPTH); shallow is the control.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("d1/d2/d3/d4/d5/d6/d7/d8/d9/.git")).unwrap();
        std::fs::create_dir_all(root.join("shallow/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.contains(&"~/shallow".to_string()), "shallow repo must be found");
        assert!(
            !out.iter().any(|s| s.ends_with("/d9")),
            "too-deep repo must not be found; got: {out:?}"
        );
    }

    #[test]
    fn given_repo_at_exact_max_depth_when_walk_then_recorded() {
        // The repo dir sits at level 8 (root=0, d1..d7=7, repo=8) — scanned at
        // depth 8. The guard is `depth > MAX_SCAN_DEPTH`, so depth == 8 is the
        // deepest level still scanned; one deeper (level 9) is rejected, as the
        // beyond_max_depth test confirms.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("d1/d2/d3/d4/d5/d6/d7/repo/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(
            out.iter().any(|s| s.ends_with("/repo")),
            "repo at exact max depth must be found; got: {out:?}"
        );
    }

    #[test]
    fn given_empty_tree_when_walk_then_out_is_empty() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("alpha/beta")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.is_empty(), "no repos means empty output; got: {out:?}");
    }

    #[test]
    fn given_sibling_repos_when_walk_then_both_recorded() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/.git")).unwrap();
        std::fs::create_dir_all(root.join("b/.git")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert!(out.contains(&"~/a".to_string()), "~/a missing; got: {out:?}");
        assert!(out.contains(&"~/b".to_string()), "~/b missing; got: {out:?}");
    }

    #[test]
    fn given_mixed_tree_when_walk_then_only_valid_repo_recorded() {
        // Pruning rules are independent: a valid sibling, a skip-dir repo, and a
        // plain nested dir coexist; only the valid sibling is reported.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("valid/.git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg/.git")).unwrap();
        std::fs::create_dir_all(root.join("src/lib")).unwrap();
        let mut out = Vec::new();
        walk_for_git(root, root, 0, &mut out);
        assert_eq!(out, vec!["~/valid".to_string()], "only valid sibling; got: {out:?}");
    }
}
