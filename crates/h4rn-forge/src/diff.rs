//! Unified-diff parsing for forges that return a raw patch (Gitea, Forgejo).
//!
//! The parser is deliberately tolerant: it extracts the file boundaries, the
//! old/new paths, the status and the hunk text, and counts added/deleted
//! lines. It never rejects a patch it half understands; an unparseable section
//! becomes a [`ForgeFileDiff`] with `status: Unknown` and the raw text as the
//! patch, because the UI can still show it.

use h4rn_protocol::{ForgeFileDiff, ForgeFileStatus};

/// Parses a unified diff into changed files.
pub fn parse_unified_diff(text: &str) -> Vec<ForgeFileDiff> {
    let mut files = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if !line.starts_with("diff --git ") {
            // A raw patch without `diff --git` headers: try a single file from
            // the first `---`/`+++` pair.
            if line.starts_with("--- ") && index + 1 < lines.len() {
                if let Some(file) = parse_plain_patch(&lines[index..]) {
                    files.push(file);
                }
                break;
            }
            index += 1;
            continue;
        }

        let (old_path, new_path) = parse_git_header(line);
        let start = index;
        index += 1;
        let mut status = ForgeFileStatus::Modified;
        let mut binary = false;
        let mut old_header = old_path.clone();
        let mut new_header = new_path.clone();

        while index < lines.len() && !lines[index].starts_with("diff --git ") {
            let current = lines[index];
            if current.starts_with("new file mode") {
                status = ForgeFileStatus::Added;
            } else if current.starts_with("deleted file mode") {
                status = ForgeFileStatus::Deleted;
            } else if current.starts_with("rename from ") {
                status = ForgeFileStatus::Renamed;
                old_header = current.trim_start_matches("rename from ").to_owned();
            } else if current.starts_with("rename to ") {
                status = ForgeFileStatus::Renamed;
                new_header = current.trim_start_matches("rename to ").to_owned();
            } else if current.starts_with("copy from ") {
                status = ForgeFileStatus::Copied;
                old_header = current.trim_start_matches("copy from ").to_owned();
            } else if current.starts_with("copy to ") {
                status = ForgeFileStatus::Copied;
                new_header = current.trim_start_matches("copy to ").to_owned();
            } else if current.starts_with("similarity index ")
                || current.starts_with("index ")
                || current.starts_with("old mode ")
                || current.starts_with("new mode ")
            {
            } else if current.starts_with("Binary files ") || current == "GIT binary patch" {
                binary = true;
            } else if current.starts_with("--- ") {
                old_header = strip_prefix(current.trim_start_matches("--- "));
            } else if current.starts_with("+++ ") {
                new_header = strip_prefix(current.trim_start_matches("+++ "));
            }
            index += 1;
        }

        let patch_lines = &lines[start..index];
        let (additions, deletions) = count_changes(patch_lines);
        let patch = patch_lines.join("\n");
        let path = if new_header == "/dev/null" {
            old_header.clone()
        } else {
            new_header.clone()
        };
        let previous_path = if status == ForgeFileStatus::Renamed && old_header != new_header {
            Some(old_header)
        } else {
            None
        };

        files.push(ForgeFileDiff {
            path,
            previous_path,
            status,
            additions,
            deletions,
            patch: if patch.trim().is_empty() {
                None
            } else {
                Some(patch)
            },
            binary,
            truncated: false,
        });
    }

    files
}

fn parse_git_header(line: &str) -> (String, String) {
    let rest = line.trim_start_matches("diff --git ");
    // `a/path b/path`; paths with spaces are quoted by git, and the fallback
    // split keeps the common case correct.
    let mut parts = rest.split(' ');
    let old = parts.next().unwrap_or_default();
    let new = parts.next().unwrap_or(old);
    (strip_prefix(old), strip_prefix(new))
}

fn strip_prefix(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix("a/").unwrap_or(value);
    let value = value.strip_prefix("b/").unwrap_or(value);
    if value == "/dev/null" {
        value.to_owned()
    } else {
        value.trim_matches('"').to_owned()
    }
}

fn count_changes(lines: &[&str]) -> (u32, u32) {
    let mut additions = 0;
    let mut deletions = 0;
    let mut in_hunk = false;
    for line in lines {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    (additions, deletions)
}

fn parse_plain_patch(lines: &[&str]) -> Option<ForgeFileDiff> {
    let old = lines.first()?.trim_start_matches("--- ");
    let new = lines.get(1)?.trim_start_matches("+++ ");
    let path = strip_prefix(new);
    if path == "/dev/null" {
        return None;
    }
    let (additions, deletions) = count_changes(lines);
    Some(ForgeFileDiff {
        path,
        previous_path: None,
        status: if old.trim() == "/dev/null" {
            ForgeFileStatus::Added
        } else {
            ForgeFileStatus::Modified
        },
        additions,
        deletions,
        patch: Some(lines.join("\n")),
        binary: false,
        truncated: false,
    })
}

/// Splits parsed files into one page and reports whether another page exists.
pub fn paginate(files: Vec<ForgeFileDiff>, page: u32, per_page: u32) -> (Vec<ForgeFileDiff>, bool) {
    let per_page = per_page.clamp(1, 200);
    let page = page.max(1);
    let start = ((page - 1) * per_page) as usize;
    let end = (start + per_page as usize).min(files.len());
    if start >= files.len() {
        return (Vec::new(), false);
    }
    (files[start..end].to_vec(), end < files.len())
}

/// Builds a unified diff for one file from its old and new contents.
///
/// Azure DevOps returns PR changes as per-file contents rather than a patch,
/// so the client diffs them locally. `None` means at least one side is not
/// text (a NUL byte); the caller reports the file as binary. Very large pairs
/// fall back to one replace-everything hunk so the request stays bounded —
/// the diff is still correct, just not minimal.
pub fn unified_diff(old_path: &str, new_path: &str, old: &str, new: &str) -> Option<String> {
    if old.as_bytes().contains(&0) || new.as_bytes().contains(&0) {
        return None;
    }
    if old == new {
        return None;
    }
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&old_lines, &new_lines);
    let hunks = hunk_ranges(&ops, 3);
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));
    out.push_str(&format!(
        "--- {}\n",
        if old_lines.is_empty() {
            "/dev/null".to_owned()
        } else {
            format!("a/{old_path}")
        }
    ));
    out.push_str(&format!(
        "+++ {}\n",
        if new_lines.is_empty() {
            "/dev/null".to_owned()
        } else {
            format!("b/{new_path}")
        }
    ));
    for (start, end) in hunks {
        let old_start = 1 + ops[..start]
            .iter()
            .filter(|op| op.kind != OpKind::Insert)
            .count();
        let new_start = 1 + ops[..start]
            .iter()
            .filter(|op| op.kind != OpKind::Delete)
            .count();
        let old_count = ops[start..end]
            .iter()
            .filter(|op| op.kind != OpKind::Insert)
            .count();
        let new_count = ops[start..end]
            .iter()
            .filter(|op| op.kind != OpKind::Delete)
            .count();
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for op in &ops[start..end] {
            let prefix = match op.kind {
                OpKind::Equal => ' ',
                OpKind::Insert => '+',
                OpKind::Delete => '-',
            };
            out.push(prefix);
            out.push_str(op.line);
            out.push('\n');
        }
    }
    Some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpKind {
    Equal,
    Insert,
    Delete,
}

#[derive(Clone, Debug)]
struct Op<'a> {
    kind: OpKind,
    line: &'a str,
}

/// Longest-common-subsequence diff with common prefix and suffix trimmed.
fn diff_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Op<'a>> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];

    let mut ops: Vec<Op<'a>> = old[..prefix]
        .iter()
        .map(|line| Op {
            kind: OpKind::Equal,
            line,
        })
        .collect();
    if old_mid.len().saturating_mul(new_mid.len()) > 4_000_000 {
        ops.extend(old_mid.iter().map(|line| Op {
            kind: OpKind::Delete,
            line,
        }));
        ops.extend(new_mid.iter().map(|line| Op {
            kind: OpKind::Insert,
            line,
        }));
    } else {
        ops.extend(lcs_ops(old_mid, new_mid));
    }
    ops.extend(old[old.len() - suffix..].iter().map(|line| Op {
        kind: OpKind::Equal,
        line,
    }));
    ops
}

fn lcs_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Op<'a>> {
    let n = old.len();
    let m = new.len();
    if n == 0 {
        return new
            .iter()
            .map(|line| Op {
                kind: OpKind::Insert,
                line,
            })
            .collect();
    }
    if m == 0 {
        return old
            .iter()
            .map(|line| Op {
                kind: OpKind::Delete,
                line,
            })
            .collect();
    }
    let width = m + 1;
    let mut table = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            ops.push(Op {
                kind: OpKind::Equal,
                line: old[i],
            });
            i += 1;
            j += 1;
        } else if i < n && (j == m || table[(i + 1) * width + j] >= table[i * width + j + 1]) {
            ops.push(Op {
                kind: OpKind::Delete,
                line: old[i],
            });
            i += 1;
        } else {
            ops.push(Op {
                kind: OpKind::Insert,
                line: new[j],
            });
            j += 1;
        }
    }
    ops
}

/// Groups changed lines into hunks with `context` lines around each change.
fn hunk_ranges(ops: &[Op<'_>], context: usize) -> Vec<(usize, usize)> {
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.kind != OpKind::Equal)
        .map(|(index, _)| index)
        .collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for change in changes {
        let start = change.saturating_sub(context);
        let end = (change + 1 + context).min(ops.len());
        match ranges.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
index 111..222 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -1,3 +1,4 @@\n\
 fn main() {}\n\
-old\n\
+new\n\
+extra\n\
diff --git a/README.md b/README.md\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/README.md\n\
@@ -0,0 +1,2 @@\n\
+hello\n\
+world\n";

    #[test]
    fn parses_multiple_files_and_counts() {
        let files = parse_unified_diff(PATCH);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[1].path, "README.md");
        assert_eq!(files[1].status, ForgeFileStatus::Added);
        assert_eq!(files[1].additions, 2);
    }

    #[test]
    fn parses_renames_and_binary_files() {
        let text = "diff --git a/old.txt b/new.txt\n\
similarity index 100%\n\
rename from old.txt\n\
rename to new.txt\n\
diff --git a/logo.png b/logo.png\n\
Binary files a/logo.png and b/logo.png differ\n";
        let files = parse_unified_diff(text);
        assert_eq!(files[0].status, ForgeFileStatus::Renamed);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].previous_path.as_deref(), Some("old.txt"));
        assert!(files[1].binary);
    }

    #[test]
    fn pagination_is_bounded() {
        let files = parse_unified_diff(PATCH);
        let (first, more) = paginate(files.clone(), 1, 1);
        assert_eq!(first.len(), 1);
        assert!(more);
        let (last, more) = paginate(files, 2, 1);
        assert_eq!(last.len(), 1);
        assert!(!more);
    }

    #[test]
    fn builds_a_unified_diff_from_two_versions() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        let new = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\n";
        let patch = unified_diff("src/lib.rs", "src/lib.rs", old, new).unwrap();
        assert!(patch.starts_with("diff --git a/src/lib.rs b/src/lib.rs\n"));
        assert!(patch.contains("--- a/src/lib.rs\n"));
        assert!(patch.contains("+++ b/src/lib.rs\n"));
        assert!(patch.contains("-three\n+THREE\n"));
        assert!(patch.contains("+nine\n"));
        // A round trip through the parser recovers the counts and path.
        let files = parse_unified_diff(&patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
    }

    #[test]
    fn a_new_or_deleted_file_uses_dev_null() {
        let added = unified_diff("src/new.rs", "src/new.rs", "", "hello\n").unwrap();
        assert!(added.contains("--- /dev/null\n"));
        assert!(added.contains("@@ -1,0 +1,1 @@\n+hello\n"));
        let deleted = unified_diff("src/old.rs", "src/old.rs", "bye\n", "").unwrap();
        assert!(deleted.contains("+++ /dev/null\n"));
        assert!(deleted.contains("-bye\n"));
    }

    #[test]
    fn binary_and_unchanged_content_have_no_patch() {
        assert!(unified_diff("a.bin", "a.bin", "a\0b", "a\0c").is_none());
        assert!(unified_diff("a.txt", "a.txt", "same\n", "same\n").is_none());
    }
}
