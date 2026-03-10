use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// Strips the common leading whitespace from every non-empty line so that
/// code extracted from inside a class or nested block is displayed at column 0.
pub fn dedent(code: &str) -> String {
    let min_indent = code
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    // When min_indent == 0, the first line may have been stripped of its
    // leading whitespace by tree-sitter (which starts byte ranges at the
    // first token, not the line start).  In that case lines 2+ can still
    // share a common indent that should be removed.
    if min_indent == 0 {
        let mut lines = code.lines();
        let first = match lines.next() {
            Some(l) => l,
            None => return code.to_owned(),
        };
        let rest_indent = lines
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);

        if rest_indent == 0 {
            return code.to_owned();
        }

        let mut result = std::iter::once(first)
            .chain(code.lines().skip(1).map(|l| {
                if l.trim().is_empty() {
                    ""
                } else {
                    &l[rest_indent..]
                }
            }))
            .collect::<Vec<_>>()
            .join("\n");

        if code.ends_with('\n') {
            result.push('\n');
        }
        return result;
    }

    let mut result = code
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[min_indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // `.lines()` + `.join()` strips the trailing newline that most source
    // files have.  Put it back so downstream consumers (syntect, callers)
    // see the same line-ending convention as the original string.
    if code.ends_with('\n') {
        result.push('\n');
    }

    result
}

/// Returns highlighted ratatui `Line`s with leading line numbers.
///
/// The `file_path` argument (full or shortened) is used to detect the language
/// by file extension.  Falls back to plain text when the extension is unknown.
pub fn highlight_code(content: &str, file_path: &str, start_line: usize) -> Vec<Line<'static>> {
    let ps = syntax_set();
    let ts = theme_set();

    // Detect language by file extension without opening the file.
    // `find_syntax_for_file` falls back to reading the first line of the file
    // when the extension is unknown, which fails for indexed files that have
    // since moved or been deleted.  Try extension first (fast path for the
    // common case, e.g. "rs" → Rust), then fall back to the full filename for
    // files without a conventional extension (e.g. "Makefile").
    let path = std::path::Path::new(file_path);
    let syntax = {
        let by_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|name| ps.find_syntax_by_extension(name))
            });
        match by_ext {
            // The "PHP" grammar (text.html.php) starts in HTML mode and needs
            // "<?php" to enter PHP mode.  Code chunks stored in the DB are
            // bare function/class bodies, never full files, so we use
            // "PHP Source" (source.php) which starts directly in PHP mode.
            Some(s) if s.name == "PHP" => ps.find_syntax_by_name("PHP Source").unwrap_or(s),
            Some(s) => s,
            None => ps.find_syntax_plain_text(),
        }
    };

    // "base16-ocean.dark" ships with syntect's default themes and works well
    // on dark terminal backgrounds (which the TUI assumes).
    let theme = &ts.themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);

    let dedented = dedent(content);
    let mut lines = Vec::new();
    for (i, raw_line) in LinesWithEndings::from(&dedented).enumerate() {
        let lineno = start_line + i;
        let ranges = h.highlight_line(raw_line, ps).unwrap_or_default();

        let mut spans: Vec<Span<'static>> = vec![Span::styled(
            format!("{lineno:>4}  "),
            Style::default().fg(Color::DarkGray),
        )];

        for (style, text) in &ranges {
            // LinesWithEndings keeps the trailing '\n'; strip it so ratatui
            // doesn't double-advance or render a blank column.
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            spans.push(Span::styled(text.to_owned(), Style::default().fg(fg)));
        }

        lines.push(Line::from(spans));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedent_basic() {
        let input = "    fn foo() {\n        let x = 1;\n    }";
        assert_eq!(dedent(input), "fn foo() {\n    let x = 1;\n}");
    }

    #[test]
    fn test_dedent_no_common_indent() {
        let input = "fn foo() {\n    let x = 1;\n}";
        assert_eq!(dedent(input), input);
    }

    #[test]
    fn test_dedent_trailing_newline_preserved() {
        let input = "    fn foo() {\n    }\n";
        assert_eq!(dedent(input), "fn foo() {\n}\n");
    }

    #[test]
    fn test_dedent_preserves_empty_lines() {
        let input = "    fn foo() {\n\n        let x = 1;\n    }";
        assert_eq!(dedent(input), "fn foo() {\n\n    let x = 1;\n}");
    }

    #[test]
    fn test_dedent_tab_indentation() {
        let input = "\tfn foo() {\n\t\tlet x = 1;\n\t}";
        assert_eq!(dedent(input), "fn foo() {\n\tlet x = 1;\n}");
    }

    #[test]
    fn test_highlight_produces_multiple_colors() {
        use ratatui::style::Color;
        let code = "fn main() {\n    let x: u32 = 42;\n    println!(\"hello\");\n}\n";
        let lines = highlight_code(code, "test.rs", 1);

        let colors: std::collections::HashSet<String> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) => Some(format!("{r},{g},{b}")),
                _ => None,
            })
            .collect();

        assert!(
            colors.len() > 1,
            "Expected multiple colors but got: {:?}",
            colors
        );
    }

    #[test]
    fn test_highlight_line_count_matches() {
        let code = "    fn foo() {\n        let x = 1;\n    }\n";
        let lines = highlight_code(code, "src/main.rs", 10);
        // 3 non-empty lines; trailing \n preserved by dedent so LinesWithEndings
        // yields exactly 3 lines (the \n on the last line doesn't add an extra one)
        assert_eq!(lines.len(), 3);
        // First line number label should be "  10  "
        assert_eq!(lines[0].spans[0].content, "  10  ");
    }

    #[test]
    fn test_highlight_php_without_preamble() {
        use ratatui::style::Color;
        // Stored DB chunks are tree-sitter nodes — they never include "<?php".
        // Highlighting must still produce multiple colors.
        let code = "public function foo() {\n    $x = 1;\n    return $x;\n}\n";
        let lines = highlight_code(code, "Home.php", 1);
        let colors: std::collections::HashSet<String> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) => Some(format!("{r},{g},{b}")),
                _ => None,
            })
            .collect();
        println!("PHP (no preamble) unique colors: {:?}", colors);
        assert!(
            colors.len() > 1,
            "PHP without preamble should produce multiple colors, got: {:?}",
            colors
        );
    }

    #[test]
    fn test_highlight_php() {
        use ratatui::style::Color;
        let code = "<?php\npublic function foo() {\n    $x = 1;\n    return $x;\n}\n";
        let lines = highlight_code(code, "Home.php", 1);
        let colors: std::collections::HashSet<String> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) => Some(format!("{r},{g},{b}")),
                _ => None,
            })
            .collect();
        println!("PHP unique colors: {:?}", colors);
        assert!(
            colors.len() > 1,
            "PHP highlighting should produce multiple colors, got: {:?}",
            colors
        );
    }

    #[test]
    fn test_dedent_treesitter_stripped_first_line() {
        // Simulate tree-sitter stripping leading whitespace from the first line:
        // the stored content has the function signature at col 0 but subsequent
        // lines retain their original file-level indentation.
        let input = "public function foo()\n    {\n        $x = 1;\n    }";
        let result = dedent(input);
        assert_eq!(result, "public function foo()\n{\n    $x = 1;\n}");
    }
}
