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
}
