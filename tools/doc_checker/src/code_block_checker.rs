// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::DocCheckerArgs;
use crate::checker::{DocCheck, DocCheckError};
use crate::md_element::Element;
use anyhow::Result;
use async_trait::async_trait;
use std::ops::Range;
use std::path::PathBuf;

const TAB_STOP: usize = 4;

/// Computes visual indentation in columns (expanding '\t' to 4-space tab stops)
/// and returns `(visual_indent, trimmed_slice)`.
fn visual_indent(line: &str) -> (usize, &str) {
    let mut indent = 0;
    let mut byte_idx = 0;
    for (idx, ch) in line.char_indices() {
        match ch {
            ' ' => {
                indent += 1;
                byte_idx = idx + 1;
            }
            '\t' => {
                indent += TAB_STOP - (indent % TAB_STOP);
                byte_idx = idx + 1;
            }
            _ => break,
        }
    }
    (indent, &line[byte_idx..])
}

/// Computes visual indentation in columns and strips optional leading blockquote
/// prefixes (`>`), returning `(total_visual_indent, remaining_slice)`.
fn strip_indent_and_blockquote(line: &str) -> (usize, &str) {
    let (mut indent, mut trimmed) = visual_indent(line);
    while let Some(after_gt) = trimmed.strip_prefix('>') {
        let (extra_indent, next_trimmed) = visual_indent(after_gt);
        indent += 1 + extra_indent;
        trimmed = next_trimmed;
    }
    (indent, trimmed)
}

/// Parses the opening fence of a code block line (` ``` ` or `~~~`), returning
/// `(base_indent, fence_char, fence_len, info_string)`.
fn parse_opening_fence(line: &str) -> Option<(usize, char, usize, &str)> {
    let (indent, trimmed) = strip_indent_and_blockquote(line);

    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        let fence_char = trimmed.chars().next()?;
        let fence_len = trimmed.chars().take_while(|&c| c == fence_char).count();
        if fence_len >= 3 {
            let remainder = trimmed[fence_len..].trim();
            if !remainder.contains(fence_char) {
                return Some((indent, fence_char, fence_len, remainder));
            }
        }
    }
    None
}

/// Checks whether `trimmed` is a closing fence matching `(fence_char, fence_len)`.
fn is_closing_fence(trimmed: &str, fence_char: char, fence_len: usize) -> bool {
    if trimmed.starts_with(fence_char) {
        let close_len = trimmed.chars().take_while(|&c| c == fence_char).count();
        if close_len >= fence_len && trimmed[close_len..].trim().is_empty() {
            return true;
        }
    }
    false
}

#[derive(Default)]
pub struct CodeBlockChecker {
    /// Tracks an orphaned closing fence expected in the same file after an indented
    /// code block inside a container was prematurely terminated by `pulldown_cmark`.
    orphaned_fence: Option<(PathBuf, char, usize)>,
}

impl CodeBlockChecker {
    fn check_element_tree(&mut self, element: &Element<'_>, errors: &mut Vec<DocCheckError>) {
        match element {
            Element::CodeBlock(info, _, doc_line, Some((file_text, range))) => {
                if let Some(block_errors) =
                    self.check_code_block(info.as_ref(), &doc_line.file_name, file_text, range)
                {
                    errors.extend(block_errors);
                }
            }
            Element::Block(_, children, _)
            | Element::Image(_, _, _, children, _)
            | Element::Link(_, _, _, children, _)
            | Element::List(_, children, _) => {
                for child in children {
                    self.check_element_tree(child, errors);
                }
            }
            _ => {}
        }
    }

    fn check_code_block(
        &mut self,
        info: &str,
        file_name: &PathBuf,
        file_text: &str,
        range: &Range<usize>,
    ) -> Option<Vec<DocCheckError>> {
        let line_start = file_text[..range.start].rfind('\n').map_or(0, |idx| idx + 1);
        let start_line_num = file_text[..line_start].bytes().filter(|&b| b == b'\n').count() + 1;

        let block_slice = &file_text[line_start..range.end];
        let block_lines: Vec<&str> = block_slice.lines().collect();
        let first_line = *block_lines.first()?;

        let (base_indent, fence_char, fence_len, _) = parse_opening_fence(first_line)?;

        let mut errors = vec![];
        let mut closed = false;
        let mut reported_inner_error = false;

        for (idx, &line) in block_lines.iter().enumerate().skip(1) {
            let current_line_num = start_line_num + idx;
            let (indent, trimmed) = strip_indent_and_blockquote(line);

            if is_closing_fence(trimmed, fence_char, fence_len) {
                closed = true;
                if indent < base_indent {
                    errors.push(DocCheckError::new_error_helpful(
                        current_line_num,
                        file_name.clone(),
                        &format!(
                            "Closing fence for code block starting at line {} has less indentation ({} spaces) than the opening fence ({} spaces).",
                            start_line_num, indent, base_indent
                        ),
                        "indenting the closing code block fence to match the opening fence.",
                    ));
                }
                break;
            }

            if trimmed.is_empty() {
                continue;
            }

            if indent < base_indent && !reported_inner_error {
                errors.push(DocCheckError::new_error_helpful(
                    current_line_num,
                    file_name.clone(),
                    &format!(
                        "Code block starting at line {} is unclosed or its inner lines are improperly indented.",
                        start_line_num
                    ),
                    "properly indenting the code block lines or closing it.",
                ));
                reported_inner_error = true;
            }
        }

        if !closed {
            // If this block is itself an orphaned bare closing fence from an earlier
            // container-terminated block in the same file, consume it without a duplicate error.
            if info.trim().is_empty()
                && self.orphaned_fence.as_ref().is_some_and(|(f, ch, len)| {
                    f == file_name && *ch == fence_char && fence_len >= *len
                })
            {
                self.orphaned_fence = None;
                return None;
            }

            // Look ahead after `range.end` to see if `pulldown_cmark` terminated the block
            // early due to an under-indented line or under-indented closing fence, or if it hit EOF.
            let trailing = &file_text[range.end..];
            let first_non_empty =
                trailing.lines().enumerate().find(|(_, line)| !line.trim().is_empty());

            if let Some((offset, next_line)) = first_non_empty {
                let offending_line_num = start_line_num + block_lines.len() + offset;
                let (next_indent, next_trimmed) = strip_indent_and_blockquote(next_line);
                self.orphaned_fence = Some((file_name.clone(), fence_char, fence_len));

                if !reported_inner_error {
                    if is_closing_fence(next_trimmed, fence_char, fence_len)
                        && next_indent < base_indent
                    {
                        errors.push(DocCheckError::new_error_helpful(
                            offending_line_num,
                            file_name.clone(),
                            &format!(
                                "Closing fence for code block starting at line {} has less indentation ({} spaces) than the opening fence ({} spaces).",
                                start_line_num, next_indent, base_indent
                            ),
                            "indenting the closing code block fence to match the opening fence.",
                        ));
                    } else {
                        errors.push(DocCheckError::new_error_helpful(
                            offending_line_num,
                            file_name.clone(),
                            &format!(
                                "Code block starting at line {} is unclosed or its inner lines are improperly indented.",
                                start_line_num
                            ),
                            "properly indenting the code block lines or closing it.",
                        ));
                    }
                }
            } else if !reported_inner_error {
                errors.push(DocCheckError::new_error_helpful(
                    start_line_num,
                    file_name.clone(),
                    &format!(
                        "Code block starting at line {} is unclosed before end of file.",
                        start_line_num
                    ),
                    "adding a matching closing code block fence.",
                ));
            }
        }

        if errors.is_empty() { None } else { Some(errors) }
    }
}

pub(crate) fn register_markdown_checks(_opt: &DocCheckerArgs) -> Result<Vec<Box<dyn DocCheck>>> {
    Ok(vec![Box::new(CodeBlockChecker::default())])
}

#[async_trait]
impl DocCheck for CodeBlockChecker {
    fn name(&self) -> &str {
        "CodeBlockChecker"
    }

    fn check(&mut self, element: &Element<'_>) -> Result<Option<Vec<DocCheckError>>> {
        let mut errors = vec![];
        self.check_element_tree(element, &mut errors);
        if errors.is_empty() { Ok(None) } else { Ok(Some(errors)) }
    }

    async fn post_check(&self) -> Result<Option<Vec<DocCheckError>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::md_element::DocContext;

    fn run_checker(markdown: &str) -> Vec<DocCheckError> {
        let mut callback = |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, markdown)
        };
        let ctx = DocContext::new(PathBuf::from("test.md"), markdown, Some(&mut callback));
        let mut checker = CodeBlockChecker::default();
        let mut errors = vec![];
        for element in ctx {
            if let Some(errs) = checker.check(&element).unwrap() {
                errors.extend(errs);
            }
        }
        errors
    }

    #[fuchsia::test]
    fn test_valid_code_block() {
        let md = r#"  ```rust
  fn main() {}
  ```
"#;
        assert!(run_checker(md).is_empty());
    }

    #[fuchsia::test]
    fn test_invalid_code_block() {
        let md = r#"  ```rust
  fn main() {}
un-indented-line
  ```
"#;
        let errors = run_checker(md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 3);
        assert!(errors[0].message.contains("Code block starting at line 1 is unclosed"));
    }

    #[fuchsia::test]
    fn test_unclosed_code_block_eof() {
        let md = r#"```rust
fn main() {}
"#;
        let errors = run_checker(md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 1);
        assert!(errors[0].message.contains("unclosed before end of file"));
    }

    #[fuchsia::test]
    fn test_code_block_nested_in_list() {
        let valid_md = r#"* {Rust}

  ```rust
  fn main() {}
  ```
"#;
        assert!(run_checker(valid_md).is_empty());

        let invalid_md = r#"* {Rust}

  ```rust
  fn main() {}
unindented
  ```
"#;
        let errors = run_checker(invalid_md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 5);
    }

    #[fuchsia::test]
    fn test_multiple_code_blocks_in_file() {
        let md = r#"* {C++}

  ```cpp
  int main() {}
  ```

* {Rust}

  ```rust
  fn main() {}
wrong_indent
  ```
"#;
        let errors = run_checker(md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 11);
    }

    #[fuchsia::test]
    fn test_under_indented_closing_fence() {
        let md = r#"* {Rust}

  ```rust
  fn main() {}
```
"#;
        let errors = run_checker(md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 5);
        assert!(errors[0].message.contains("Closing fence for code block starting at line 3"));
    }

    #[fuchsia::test]
    fn test_html_comment_code_fence_ignored() {
        let md = r#"<!--
  ```rust
unindented
-->

```rust
fn main() {}
```
"#;
        assert!(run_checker(md).is_empty());
    }

    #[fuchsia::test]
    fn test_tab_indentation() {
        let md = "\t```rust\n    fn main() {}\n\t```\n";
        assert!(run_checker(md).is_empty());
    }

    #[fuchsia::test]
    fn test_code_block_in_blockquote() {
        let valid_md = r#"> ```rust
> fn main() {}
>
> ```
"#;
        assert!(run_checker(valid_md).is_empty());

        let invalid_md = r#">   ```rust
> fn main() {}
>   ```
"#;
        let errors = run_checker(invalid_md);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].doc_line.line_num, 2);
    }
}
