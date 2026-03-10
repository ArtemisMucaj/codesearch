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

    if min_indent == 0 {
        return code.to_owned();
    }

    code.lines()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[min_indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns highlighted ratatui `Line`s with leading line numbers.
///
/// The `file_path` argument (full or shortened) is used to detect the language
/// by file extension.  Falls back to plain text when the extension is unknown.
pub fn highlight_code(content: &str, file_path: &str, start_line: usize) -> Vec<Line<'static>> {
    let ps = syntax_set();
    let ts = theme_set();

    let syntax = ps
        .find_syntax_for_file(file_path)
        .ok()
        .flatten()
        .unwrap_or_else(|| ps.find_syntax_plain_text());

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
