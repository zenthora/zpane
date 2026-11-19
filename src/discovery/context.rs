use super::catalog::{cmp_ascii_case_insensitive, read_bounded_string_lossy, ShellDialect};
use super::config::load_user_config;
use super::history::detect_shell_history;
use crate::model::{CwdEntry, RuntimeContext};
use crate::sanitize::{has_disallowed_control, sanitize_single_line};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_GIT_SMALL_BYTES: u64 = 64 * 1024;
const MAX_GIT_PACKED_REFS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_GIT_REFS_DEPTH: usize = 32;
const MAX_GIT_REFS_COUNT: usize = 10_000;
const MAX_CWD_SCAN_READS: usize = 10_000;

/// Detects current Git branch/branches, shallow CWD entries, and shell history
/// to populate [`RuntimeContext`]. Any fields already provided in `explicit` are preserved.
pub fn detect_runtime_context(explicit: RuntimeContext) -> RuntimeContext {
    let mut ctx = explicit;
    if ctx.history.is_empty() {
        let limit = load_user_config().history_limit.unwrap_or(15);
        ctx.history = detect_shell_history(ShellDialect::detect(), limit);
    }

    let Ok(cwd) = std::env::current_dir() else {
        return ctx;
    };

    if ctx.git_branch.is_none() || ctx.git_branches.is_empty() {
        let (detected_branch, detected_branches) = detect_git_context_in(&cwd);
        if ctx.git_branch.is_none() {
            ctx.git_branch = detected_branch;
        }
        if ctx.git_branches.is_empty() {
            ctx.git_branches = detected_branches;
        }
    }

    if let Some(ref current) = ctx.git_branch {
        if !ctx.git_branches.iter().any(|b| b == current) {
            ctx.git_branches.insert(0, current.clone());
        }
    }

    if ctx.cwd_entries.is_empty() {
        ctx.cwd_entries = scan_cwd_entries(&cwd);
    }

    ctx
}

fn resolve_git_dir(start_dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            if let Some(content) = read_bounded_string_lossy(&candidate, MAX_GIT_SMALL_BYTES) {
                if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                    let trimmed = rest.trim();
                    if !trimmed.is_empty() && !trimmed.contains('\0') {
                        let git_path = PathBuf::from(trimmed);
                        return Some(if git_path.is_absolute() {
                            git_path
                        } else {
                            dir.join(git_path)
                        });
                    }
                }
            }
        }
        cur = dir.parent();
    }
    None
}

fn collect_git_refs(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth >= MAX_GIT_REFS_DEPTH || out.len() >= MAX_GIT_REFS_COUNT {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_GIT_REFS_COUNT {
            break;
        }
        // Use `entry.file_type()` so symlinks are never followed into directory loops.
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_git_refs(base, &path, depth + 1, out);
        } else if ft.is_file() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".lock"))
            {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(base) {
                let raw_branch = rel.to_string_lossy().replace('\\', "/");
                let clean = sanitize_single_line(&raw_branch);
                if !clean.is_empty() {
                    out.push(clean.into_owned());
                }
            }
        }
    }
}

fn resolve_git_common_dir(git_dir: &Path) -> PathBuf {
    let Some(raw) = read_bounded_string_lossy(&git_dir.join("commondir"), MAX_GIT_SMALL_BYTES)
    else {
        return git_dir.to_path_buf();
    };
    let rel = raw.trim();
    if rel.is_empty() || rel.contains('\0') {
        return git_dir.to_path_buf();
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        git_dir.join(candidate)
    }
}

fn detect_git_context_in(start_dir: &Path) -> (Option<String>, Vec<String>) {
    let Some(git_dir) = resolve_git_dir(start_dir) else {
        return (None, Vec::new());
    };
    let common_dir = resolve_git_common_dir(&git_dir);

    let current =
        read_bounded_string_lossy(&git_dir.join("HEAD"), MAX_GIT_SMALL_BYTES).and_then(|head| {
            let trimmed = head.trim();
            let raw = if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                Some(branch.to_string())
            } else if let Some(other_ref) = trimmed.strip_prefix("ref: ") {
                Some(other_ref.to_string())
            } else if !trimmed.is_empty() {
                Some(trimmed.chars().take(8).collect())
            } else {
                None
            }?;
            let clean = sanitize_single_line(&raw);
            (!clean.is_empty()).then(|| clean.into_owned())
        });

    let mut branches = Vec::new();
    let heads_dir = common_dir.join("refs").join("heads");
    if heads_dir.is_dir() {
        collect_git_refs(&heads_dir, &heads_dir, 0, &mut branches);
    }
    if common_dir != git_dir {
        let local_heads = git_dir.join("refs").join("heads");
        if local_heads.is_dir() {
            collect_git_refs(&local_heads, &local_heads, 0, &mut branches);
        }
    }
    let mut packed_seen = HashSet::new();
    for dir in [&common_dir, &git_dir] {
        let packed_path = dir.join("packed-refs");
        if !packed_seen.insert(packed_path.clone()) {
            continue;
        }
        if let Some(packed) = read_bounded_string_lossy(&packed_path, MAX_GIT_PACKED_REFS_BYTES) {
            for line in packed.lines() {
                if branches.len() >= MAX_GIT_REFS_COUNT {
                    break;
                }
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((_, ref_name)) = line.split_once(char::is_whitespace) {
                    if let Some(branch) = ref_name.trim().strip_prefix("refs/heads/") {
                        let clean = sanitize_single_line(branch);
                        if !clean.is_empty() {
                            branches.push(clean.into_owned());
                        }
                    }
                }
            }
        }
    }
    branches.sort();
    branches.dedup();

    if let Some(ref cur) = current {
        branches.retain(|b| b != cur);
        branches.insert(0, cur.clone());
    }

    (current, branches)
}

fn scan_cwd_entries(dir: &Path) -> Vec<CwdEntry> {
    const MAX_CWD_ENTRIES: usize = 200;
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<CwdEntry> = rd
        .take(MAX_CWD_SCAN_READS)
        .flatten()
        .filter_map(|e| {
            let os_name = e.file_name();
            let name_str = os_name.to_str()?;
            if name_str.is_empty() || has_disallowed_control(name_str) {
                return None;
            }
            let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            let name = os_name.into_string().ok()?;
            Some(CwdEntry { name, is_dir })
        })
        .collect();
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| cmp_ascii_case_insensitive(&a.name, &b.name))
    });
    items.truncate(MAX_CWD_ENTRIES);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_detect_git_context_and_cwd_scan_in_temp_dir() {
        let base = env::temp_dir().join(format!("zpane_ctx_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".git").join("refs").join("heads").join("feat")).unwrap();
        fs::create_dir_all(base.join("subdir").join("nested")).unwrap();
        fs::write(base.join("alpha.txt"), b"hello").unwrap();
        fs::write(
            base.join(".git").join("HEAD"),
            b"ref: refs/heads/feat/demo\n",
        )
        .unwrap();
        fs::write(
            base.join(".git").join("refs").join("heads").join("main"),
            b"0123456789abcdef\n",
        )
        .unwrap();
        fs::write(
            base.join(".git")
                .join("refs")
                .join("heads")
                .join("feat")
                .join("demo"),
            b"fedcba9876543210\n",
        )
        .unwrap();

        let (branch, branches) = detect_git_context_in(&base.join("subdir").join("nested"));
        assert_eq!(branch.as_deref(), Some("feat/demo"));
        assert_eq!(branches, vec!["feat/demo", "main"]);

        let cwd = scan_cwd_entries(&base);
        let names: Vec<(&str, bool)> = cwd.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(
            names,
            vec![(".git", true), ("subdir", true), ("alpha.txt", false)]
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_detect_git_context_resolves_worktree_commondir_and_skips_lock_files() {
        let root = env::temp_dir().join(format!("zpane_git_wt_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let main_git = root.join("repo").join(".git");
        let wt_git = main_git.join("worktrees").join("wt1");
        let wt_dir = root.join("wt1_checkout");

        fs::create_dir_all(main_git.join("refs").join("heads").join("feature")).unwrap();
        fs::create_dir_all(&wt_git).unwrap();
        fs::create_dir_all(wt_dir.join("sub")).unwrap();

        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", wt_git.display()),
        )
        .unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();

        fs::write(main_git.join("refs").join("heads").join("main"), b"0001\n").unwrap();
        fs::write(
            main_git.join("refs").join("heads").join("main.lock"),
            b"lock\n",
        )
        .unwrap();
        fs::write(
            main_git.join("packed-refs"),
            "# pack-refs\nabcdef0123456789\trefs/heads/feature/wt \n^000000\n",
        )
        .unwrap();

        let (current, branches) = detect_git_context_in(&wt_dir.join("sub"));
        assert_eq!(current.as_deref(), Some("wt-branch"));
        assert_eq!(branches, vec!["wt-branch", "feature/wt", "main"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_git_refs_traversal_stops_at_max_depth() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("zpane_git_depth_{}_{nonce}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let git_dir = root.join("repo").join(".git");
        let heads = git_dir.join("refs").join("heads");
        fs::create_dir_all(&heads).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(heads.join("main"), "0123456789abcdef\n").unwrap();

        let mut deep = heads.clone();
        for i in 0..40 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("too_deep"), "0123456789abcdef\n").unwrap();

        let (git_branch, git_branches) = detect_git_context_in(&root.join("repo"));
        assert_eq!(git_branch.as_deref(), Some("main"));
        assert!(git_branches.contains(&"main".to_string()));
        assert!(!git_branches.iter().any(|b| b.contains("too_deep")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_pentest_cwd_scan_filters_hostile_entries() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "zpane_test_cwd_pentest_{}_{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub_dir")).unwrap();
        fs::write(root.join("safe.txt"), b"ok").unwrap();

        // Hostile Unicode BiDi and line-separator filenames must be skipped before allocation.
        let _ = fs::write(root.join("spoof\u{202E}txt.exe"), b"bad");
        let _ = fs::write(root.join("line\u{2028}sep.txt"), b"bad");

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            let raw_invalid =
                OsString::from_vec(vec![b'b', b'a', b'd', 0xff, b'.', b't', b'x', b't']);
            let _ = fs::write(root.join(raw_invalid), b"bad");
        }

        let cwd = scan_cwd_entries(&root);
        let names: Vec<(&str, bool)> = cwd.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(names, vec![("sub_dir", true), ("safe.txt", false)]);
        let _ = fs::remove_dir_all(&root);
    }
}
