// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashSet;

/// Marker separating a rolled commit's message from the roll commit's own footers.
const PRIVATE_MARKER: &str = "---PRIVATE---";

/// Footers that mark the end of a rolled commit's footers within a roll commit message.
const ROLLER_BOUNDARY_FOOTERS: &[&str] = &[
    "Original-Reviewed-on",
    "Original-Original-Reviewed-on",
    "Original-Revision",
    "Original-Original-Revision",
    "Rolled-Repo",
    "Rolled-Commits",
    "Roller-URL",
];

/// Parsed commit message containing subject, body, footers, and extracted dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommitMessage {
    pub subject: String,
    pub body: String,
    pub raw_message: String,
    pub footers: Vec<(String, String)>,
    pub depends_on: Vec<String>,
    pub change_id: Option<String>,
}

impl ParsedCommitMessage {
    /// Parses a git commit message into subject, body, footers, and dependencies.
    pub fn parse(raw_message: &str) -> Self {
        let trimmed = raw_message.trim();
        let lines: Vec<&str> = trimmed.lines().collect();
        if lines.is_empty() {
            return Self {
                subject: String::new(),
                body: String::new(),
                raw_message: raw_message.to_string(),
                footers: Vec::new(),
                depends_on: Vec::new(),
                change_id: None,
            };
        }

        let subject = lines[0].trim().to_string();
        let mut footers = Vec::new();
        let mut depends_on = Vec::new();
        let mut change_id = None;

        // Split paragraphs (separated by blank lines)
        let paragraphs: Vec<&[&str]> =
            lines.split(|l| l.trim().is_empty()).filter(|p| !p.is_empty()).collect();

        // Scan backwards through trailing paragraphs that consist entirely of footer lines.
        // Roll commits embed the rolled commit's message (including its footers) above a
        // `---PRIVATE---` marker, so never treat anything above that marker as footers of the
        // outer commit. For example, an integration roll commit message looks like:
        //
        //     [roll] Roll fuchsia [foo] Fix README typo
        //
        //     Fix a typo in the foo README.
        //
        //     Depends-on: I1111111111111111111111111111111111111111
        //     Original-Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1234567
        //     Original-Revision: 0123456789abcdef0123456789abcdef01234567
        //     ---PRIVATE---
        //
        //     Rolled-Repo: https://fuchsia.googlesource.com/fuchsia
        //     Change-Id: I2222222222222222222222222222222222222222
        //     Reviewed-on: https://turquoise-internal-review.googlesource.com/c/integration/+/7654
        //
        // Only the footers below the marker (`Rolled-Repo`, `Change-Id`, `Reviewed-on`) belong to
        // the roll commit; the rolled commit's footers above it (including its `Depends-on`) do
        // not.
        let mut footer_paragraphs = Vec::new();
        for para in paragraphs.iter().skip(1).rev() {
            if let Some(marker_idx) = para.iter().rposition(|l| l.contains(PRIVATE_MARKER)) {
                let below_marker = &para[marker_idx + 1..];
                if !below_marker.is_empty()
                    && below_marker.iter().all(|line| parse_footer_line(line).is_some())
                {
                    footer_paragraphs.push(below_marker);
                }
                break;
            }
            let all_footers = para.iter().all(|line| parse_footer_line(line).is_some());
            if all_footers && !para.is_empty() {
                footer_paragraphs.push(*para);
            } else {
                break;
            }
        }
        footer_paragraphs.reverse();

        for para in footer_paragraphs {
            for line in para {
                if let Some((k, v)) = parse_footer_line(line) {
                    footers.push((k, v));
                }
            }
        }

        // Copybara strips `---PRIVATE---` when exporting roll commits, merging the rolled
        // commit's footers into the same block as the roll commit's own footers. Any
        // `Depends-on` preceding a roller/original-commit boundary footer belongs to the rolled
        // commit rather than to this commit.
        let boundary_idx = footers
            .iter()
            .rposition(|(k, _)| ROLLER_BOUNDARY_FOOTERS.iter().any(|b| k.eq_ignore_ascii_case(b)));
        for (idx, (k, v)) in footers.iter().enumerate() {
            if k.eq_ignore_ascii_case("Depends-on") {
                if boundary_idx.is_none_or(|b| idx > b) {
                    depends_on.push(v.clone());
                }
            } else if k.eq_ignore_ascii_case("Change-Id") {
                change_id = Some(v.clone());
            }
        }

        let body =
            if lines.len() > 1 { lines[1..].join("\n").trim().to_string() } else { String::new() };

        Self { subject, body, raw_message: raw_message.to_string(), footers, depends_on, change_id }
    }
}

fn parse_footer_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let colon_idx = trimmed.find(':')?;
    let key = trimmed[..colon_idx].trim();
    // Keys in git trailers typically contain alphanumeric and dashes
    if key.is_empty() || key.contains(' ') || !key.chars().all(|c| c.is_alphanumeric() || c == '-')
    {
        return None;
    }
    let value = trimmed[colon_idx + 1..].trim();
    Some((key.to_string(), value.to_string()))
}

/// Formats a pure revert commit message adhering to Gerrit and Rubber Stamper rules.
///
/// If `new_change_id` is `None`, no `Change-Id` footer is emitted; Gerrit generates one
/// server-side when the revert change is created through its REST API.
///
/// Note: `Bug:` lines in the reason are stripped from the reason block
/// and emitted in the bottom git trailer block instead.
pub fn format_pure_revert_message(
    original_commit_sha: &str,
    original_subject: &str,
    original_message: &str,
    reason: &str,
    depends_on: &[String],
    new_change_id: Option<&str>,
) -> String {
    let clean_subj = original_subject.trim();
    let revert_subject = format!("Revert \"{clean_subj}\"");

    let mut quoted_lines = Vec::new();
    for line in original_message.trim().lines() {
        if line.trim().is_empty() {
            quoted_lines.push(">".to_string());
        } else {
            quoted_lines.push(format!("> {}", line));
        }
    }
    let quoted_description = quoted_lines.join("\n");

    let mut clean_reason = Vec::new();
    let mut bug_lines = Vec::new();
    let mut seen_bugs = HashSet::new();
    for line in reason.lines() {
        if let Some((key, value)) = parse_footer_line(line)
            && key.eq_ignore_ascii_case("Bug")
        {
            if !value.is_empty() && seen_bugs.insert(value.clone()) {
                bug_lines.push(format!("Bug: {}", value));
            }
            continue;
        }
        clean_reason.push(line);
    }

    let mut collapsed_reason = Vec::new();
    let mut last_was_empty = false;
    for line in clean_reason {
        let is_empty = line.trim().is_empty();
        if is_empty && last_was_empty {
            continue;
        }
        collapsed_reason.push(line);
        last_was_empty = is_empty;
    }
    let reason_text = collapsed_reason.join("\n").trim().to_string();

    let mut footers = Vec::new();
    footers.extend(bug_lines);
    let mut seen_deps = HashSet::new();
    for dep in depends_on {
        let dep = dep.trim();
        if !dep.is_empty() && seen_deps.insert(dep.to_string()) {
            footers.push(format!("Depends-on: {}", dep));
        }
    }
    if let Some(cid) = new_change_id {
        footers.push(format!("Change-Id: {}", cid));
    }

    let reason_block = if reason_text.starts_with("Reason for revert:") {
        reason_text
    } else {
        format!("Reason for revert:\n{}", reason_text)
    };

    let mut message = format!(
        "{revert_subject}\n\nThis reverts commit {original_commit_sha}.\n\n{reason_block}\n\nOriginal change's description:\n{quoted_description}\n"
    );
    if !footers.is_empty() {
        message.push('\n');
        message.push_str(&footers.join("\n"));
        message.push('\n');
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_parse_commit_with_colon_in_subject_only() {
        let msg = ParsedCommitMessage::parse("starnix: fix memory leak");
        assert_eq!(msg.subject, "starnix: fix memory leak");
        assert_eq!(msg.body, "");
        assert!(msg.footers.is_empty());
    }

    #[fuchsia::test]
    fn test_parse_commit_message_footers() {
        let msg = r#"[fidl] Update protocol

This updates the fidl protocol for testing.

Depends-on: turquoise-internal:I1234567890abcdef1234567890abcdef12345678
Change-Id: Iabcdef1234567890abcdef1234567890abcdef12"#;

        let parsed = ParsedCommitMessage::parse(msg);
        assert_eq!(parsed.subject, "[fidl] Update protocol");
        assert_eq!(
            parsed.depends_on,
            vec!["turquoise-internal:I1234567890abcdef1234567890abcdef12345678"]
        );
        assert_eq!(parsed.change_id.as_deref(), Some("Iabcdef1234567890abcdef1234567890abcdef12"));
    }

    #[fuchsia::test]
    fn test_parse_multi_paragraph_footers() {
        let msg = r#"[core] Refactor service

Detailed explanation of the change.

Depends-on: turquoise-internal:I1111111111111111111111111111111111111111
Change-Id: I2222222222222222222222222222222222222222

Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/123456
Reviewed-by: Alice <alice@google.com>
Commit-Queue: Bob <bob@google.com>"#;

        let parsed = ParsedCommitMessage::parse(msg);
        assert_eq!(parsed.subject, "[core] Refactor service");
        assert_eq!(
            parsed.depends_on,
            vec!["turquoise-internal:I1111111111111111111111111111111111111111"]
        );
        assert_eq!(parsed.change_id.as_deref(), Some("I2222222222222222222222222222222222222222"));
        assert_eq!(parsed.footers.len(), 5);
    }

    #[fuchsia::test]
    fn test_parse_private_roll_commit_ignores_rolled_footers() {
        // turquoise-internal-review roll commit: the rolled CL's message and footers appear
        // above `---PRIVATE---`, and the roll commit's own footers appear below it.
        let msg = r#"[roll] Roll fuchsia [diagnostics][inspect-fetcher] Fix README typo

Fix a typo in the inspect-fetcher README.

Original-Original-Bug: None
Test: None, documentation change.
Depends-on: Iff33face412aeaa25007911f72c624c53dc4e1da
Original-Original-Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1885744
Original-Original-Revision: 9556313fb2eed72ff65c2c7778540a69a7564e0f
---PRIVATE---

Rolled-Repo: https://fuchsia.googlesource.com/fuchsia
Rolled-Commits: 851c282b4ac521..9556313fb2eed7
Roller-URL: https://cr-buildbucket.appspot.com/build/8668054359904622977
CQ-Do-Not-Cancel-Tryjobs: true
Change-Id: I521ca48e12f61d133b414b0a05d784a546628a63
Original-Reviewed-on: https://turquoise-internal-review.googlesource.com/c/integration/+/1690837
GitOrigin-RevId: d6e2c11553eb4cd626c51ef8ec216594e823df3b
Reviewed-on: https://turquoise-internal-review.googlesource.com/c/smart-integration/+/1685575
Cr-Commit-Position: refs/heads/main@{#127603}"#;

        let parsed = ParsedCommitMessage::parse(msg);
        assert!(parsed.depends_on.is_empty(), "{:?}", parsed.depends_on);
        assert_eq!(parsed.change_id.as_deref(), Some("I521ca48e12f61d133b414b0a05d784a546628a63"));
        assert_eq!(parsed.footers.len(), 9);
        assert!(!parsed.footers.iter().any(|(k, _)| k == "Depends-on"));

        // `---PRIVATE---` sharing a paragraph with the roll commit's footers.
        let msg = "[roll] Roll fuchsia Foo\n\nDepends-on: I_rolled\n---PRIVATE---\n\
                   Rolled-Repo: https://fuchsia.googlesource.com/fuchsia\nChange-Id: I_roll";
        let parsed = ParsedCommitMessage::parse(msg);
        assert!(parsed.depends_on.is_empty(), "{:?}", parsed.depends_on);
        assert_eq!(parsed.change_id.as_deref(), Some("I_roll"));
        assert_eq!(parsed.footers.len(), 2);
    }

    #[fuchsia::test]
    fn test_parse_copybara_roll_commit_ignores_rolled_depends_on() {
        // fuchsia-review roll commit exported by Copybara: `---PRIVATE---` is stripped and the
        // rolled CL's footers are merged into the roll commit's footer block.
        let msg = r#"[roll] Roll fuchsia [diagnostics][inspect-fetcher] Fix README typo

Fix a typo in the inspect-fetcher README.

Depends-on: Iff33face412aeaa25007911f72c624c53dc4e1da
Original-Original-Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1885744
Original-Original-Revision: 9556313fb2eed72ff65c2c7778540a69a7564e0f
GitOrigin-RevId: 4f389ea7ef68aa35520a79ca3cc52e63d6d24996
Change-Id: I521ca48e12f61d133b414b0a05d784a546628a63
Reviewed-on: https://fuchsia-review.googlesource.com/c/integration/+/1888500"#;

        let parsed = ParsedCommitMessage::parse(msg);
        assert!(parsed.depends_on.is_empty(), "{:?}", parsed.depends_on);
        assert_eq!(parsed.change_id.as_deref(), Some("I521ca48e12f61d133b414b0a05d784a546628a63"));
        assert_eq!(parsed.footers.len(), 6);
    }

    #[fuchsia::test]
    fn test_parse_depends_on_after_roller_boundary_is_kept() {
        // Normal CL whose own footer block has no roller boundary markers.
        let msg = "[core] Change\n\nBody.\n\nDepends-on: fuchsia:I_dep\nChange-Id: I_self\n\n\
                   Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1\n\
                   GitOrigin-RevId: abc";
        let parsed = ParsedCommitMessage::parse(msg);
        assert_eq!(parsed.depends_on, vec!["fuchsia:I_dep"]);
        assert_eq!(parsed.change_id.as_deref(), Some("I_self"));

        // Depends-on footers after the last boundary footer belong to the outer commit.
        let msg = "[core] Change\n\nDepends-on: I_rolled\nOriginal-Revision: abc\n\
                   Depends-on: turquoise-internal:I_own\nChange-Id: I_self";
        let parsed = ParsedCommitMessage::parse(msg);
        assert_eq!(parsed.depends_on, vec!["turquoise-internal:I_own"]);
        assert_eq!(parsed.change_id.as_deref(), Some("I_self"));
    }

    #[fuchsia::test]
    fn test_format_pure_revert_message() {
        let original_sha = "0123456789abcdef0123456789abcdef01234567";
        let original_subj = "[core] Fix memory leak";
        let original_msg = "[core] Fix memory leak\n\nFixed a leak in handler.\n\nChange-Id: Iold";
        let reason = "Causing flakes in CI";
        let deps = vec!["turquoise-internal:Inew_dep".to_string()];
        let new_cid = "Inew_cid_1234567890abcdef1234567890abcdef12";

        let formatted = format_pure_revert_message(
            original_sha,
            original_subj,
            original_msg,
            reason,
            &deps,
            Some(new_cid),
        );

        assert!(formatted.starts_with("Revert \"[core] Fix memory leak\"\n\n"));
        assert!(
            formatted.contains("This reverts commit 0123456789abcdef0123456789abcdef01234567.")
        );
        assert!(formatted.contains("Reason for revert:\nCausing flakes in CI"));
        assert!(formatted.contains("> [core] Fix memory leak\n>\n> Fixed a leak in handler."));
        assert!(formatted.contains("Depends-on: turquoise-internal:Inew_dep\nChange-Id: Inew_cid_1234567890abcdef1234567890abcdef12"));
    }

    #[fuchsia::test]
    fn test_format_pure_revert_message_with_bug() {
        let original_sha = "0123456789abcdef0123456789abcdef01234567";
        let original_subj = "[core] Fix memory leak";
        let original_msg = "[core] Fix memory leak\n\nFixed a leak in handler.\n\nChange-Id: Iold";
        let reason = "Causing flakes in CI\n\nBug: 12345\nbug: 12345\n\nBUG: 67890";
        let deps = vec!["turquoise-internal:Inew_dep".to_string()];
        let new_cid = "Inew_cid_1234567890abcdef1234567890abcdef12";

        let formatted = format_pure_revert_message(
            original_sha,
            original_subj,
            original_msg,
            reason,
            &deps,
            Some(new_cid),
        );

        assert!(formatted.starts_with("Revert \"[core] Fix memory leak\"\n\n"));
        assert!(formatted.contains("Reason for revert:\nCausing flakes in CI\n\nOriginal"));
        assert!(formatted.contains("\nBug: 12345\nBug: 67890\nDepends-on: turquoise-internal:Inew_dep\nChange-Id: Inew_cid_1234567890abcdef1234567890abcdef12"));
    }

    #[fuchsia::test]
    fn test_format_pure_revert_message_double_revert() {
        let original_sha = "0123456789abcdef0123456789abcdef01234567";
        let original_subj = "Revert \"Foo\"";
        let original_msg = "Revert \"Foo\"\n\nChange-Id: Iold";
        let reason = "Reverting the revert";
        let deps = vec![];
        let new_cid = "Inew_cid_1234567890abcdef1234567890abcdef12";

        let formatted = format_pure_revert_message(
            original_sha,
            original_subj,
            original_msg,
            reason,
            &deps,
            Some(new_cid),
        );

        assert!(formatted.starts_with(
            r#"Revert "Revert "Foo""

"#
        ));
    }

    #[fuchsia::test]
    fn test_format_pure_revert_message_without_change_id() {
        let formatted = format_pure_revert_message(
            "0123456789abcdef0123456789abcdef01234567",
            "[core] Fix memory leak",
            "[core] Fix memory leak\n\nChange-Id: Iold",
            "Reason for revert: Relanding",
            &["turquoise-internal:Idep".to_string(), "turquoise-internal:Idep".to_string()],
            None,
        );

        assert!(formatted.starts_with("Revert \"[core] Fix memory leak\"\n\n"));
        assert!(formatted.contains("\n\nReason for revert: Relanding\n\n"));
        // The original Change-Id is only present as quoted text, never as a footer.
        assert!(formatted.contains("> Change-Id: Iold"));
        assert!(!formatted.contains("\nChange-Id: "), "{formatted}");
        assert_eq!(formatted.matches("Depends-on: turquoise-internal:Idep").count(), 1);
        assert!(formatted.ends_with("\n\nDepends-on: turquoise-internal:Idep\n"), "{formatted}");

        let no_footers = format_pure_revert_message("abc1234", "S", "S", "R", &[], None);
        assert!(no_footers.ends_with("Original change's description:\n> S\n"), "{no_footers}");
    }
}
