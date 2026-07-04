/// Builds a RE2/POSIX-ERE substring pattern that tolerates stripped namespace
/// separators caused by shell quoting.
///
/// When a user types a PHP FQN like `Acme\Models\Homes\Home#method` in an
/// *unquoted* shell argument the shell strips the backslashes, yielding
/// `AcmeModelsHomesHome#method`.  This function generates a pattern that
/// matches **both** forms by:
///
/// 1. Converting any separator character (`\`, `/`, `#`, `.`, `:`) already present
///    in the input into the flexible class `[/\\#.:]*` (zero or more of any
///    separator).
/// 2. Inserting `[/\\#.:]*` at lowercase→uppercase PascalCase word boundaries
///    so that shell-stripped segments (e.g. `AcmeModels`) also match their
///    separated form (e.g. `Acme\Models`).
///
/// All other POSIX ERE metacharacters are escaped so they match literally.
pub fn build_fuzzy_pattern(s: &str) -> String {
    /// RE2 character class matching 0 or more of `/`, `\`, `#`, `.`, `:`
    const SEP: &str = r"[/\\#.:]*";

    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        match c {
            // Existing separator char → flexible separator class.
            '\\' | '/' | '#' | '.' | ':' => {
                // Avoid two consecutive SEP tokens.
                if !out.ends_with(SEP) {
                    out.push_str(SEP);
                }
            }
            // PascalCase boundary (lowercase → uppercase) → optional separator.
            c if c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase() => {
                if !out.ends_with(SEP) {
                    out.push_str(SEP);
                }
                out.push(c);
            }
            // POSIX ERE metacharacters (other than the separators above) → escape.
            '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' => {
                out.push('\\');
                out.push(c);
            }
            // Regular character.
            c => out.push(c),
        }
    }

    out
}

/// Split a fully-qualified symbol name into its short (unqualified) member name
/// and an optional class/file hint for disambiguation.
///
/// A trailing SCIP call-signature suffix (`()` or `().`) is stripped first.
/// Separators are then checked in precedence order: `#`, `\`, `::`, `.`, `/`.
/// - `Namespace\Class#method` → `("method", Some("Class"))`
/// - `src/foo/Bar#baz().`     → `("baz",    Some("Bar"))`
/// - `Namespace\Class`        → `("Class",  None)`  — `\` is namespace-only
/// - `crate::module::fn`      → `("fn",     Some("module"))`
/// - `com.example.Foo`        → `("Foo",    Some("example"))`
/// - `bare`                   → `("bare",   None)`
///
/// The hint is `None` when the separator is `\` (pure namespace, no method
/// context) or when there is no separator at all.
pub(crate) fn parse_fqn(symbol: &str) -> (&str, Option<&str>) {
    let symbol = symbol
        .strip_suffix("().")
        .or_else(|| symbol.strip_suffix("()"))
        .unwrap_or(symbol);
    if let Some(pos) = symbol.rfind('#') {
        return (&symbol[pos + 1..], extract_class_hint(&symbol[..pos]));
    }
    if let Some(pos) = symbol.rfind('\\') {
        // Backslash is a namespace-only separator: gives a short name but no hint.
        return (&symbol[pos + 1..], None);
    }
    if let Some(pos) = symbol.rfind("::") {
        return (&symbol[pos + 2..], extract_class_hint(&symbol[..pos]));
    }
    if let Some(pos) = symbol.rfind('.') {
        return (&symbol[pos + 1..], extract_class_hint(&symbol[..pos]));
    }
    if let Some(pos) = symbol.rfind('/') {
        return (&symbol[pos + 1..], extract_class_hint(&symbol[..pos]));
    }
    (symbol, None)
}

/// Strip leading namespace prefixes (`\`, `::`, `.`, `/`) from `class_part` and
/// return the last unqualified segment, or `None` if the result is empty.
fn extract_class_hint(class_part: &str) -> Option<&str> {
    let start = class_part
        .rfind('\\')
        .or_else(|| class_part.rfind("::").map(|p| p + 1))
        .or_else(|| class_part.rfind('.'))
        .or_else(|| class_part.rfind('/'))
        .map(|p| p + 1)
        .unwrap_or(0);
    let hint = &class_part[start..];
    if hint.is_empty() {
        None
    } else {
        Some(hint)
    }
}

/// Extract the short (unqualified) name from a fully-qualified symbol.
pub(crate) fn short_symbol_name(symbol: &str) -> &str {
    parse_fqn(symbol).0
}

/// Extract a class/file hint from a fully-qualified symbol for disambiguation.
///
/// Returns `None` when no useful class hint can be derived (e.g. bare symbol
/// or a backslash-only namespace path without a method separator).
pub(crate) fn class_hint_from_symbol(symbol: &str) -> Option<&str> {
    parse_fqn(symbol).1
}

#[cfg(test)]
mod fqn_tests {
    use super::*;

    #[test]
    fn test_short_symbol_name() {
        assert_eq!(short_symbol_name("Namespace\\Class#method"), "method");
        assert_eq!(short_symbol_name("Namespace\\Class"), "Class");
        assert_eq!(short_symbol_name("crate::module::func"), "func");
        assert_eq!(short_symbol_name("com.example.Foo"), "Foo");
        assert_eq!(short_symbol_name("bare"), "bare");
        // SCIP call-signature suffixes are stripped before splitting.
        assert_eq!(short_symbol_name("src/foo/Bar#baz()."), "baz");
        assert_eq!(short_symbol_name("Autoloader#load()"), "load");
        // Path-qualified without a method separator.
        assert_eq!(short_symbol_name("src/foo/bar"), "bar");
        // Malformed: separator at the end → empty short name
        assert_eq!(short_symbol_name("Class#"), "");
        assert_eq!(short_symbol_name("module::"), "");
        assert_eq!(short_symbol_name("pkg."), "");
        assert_eq!(short_symbol_name("Ns\\"), "");
    }

    #[test]
    fn test_class_hint_from_symbol() {
        // '#' separator (SCIP / PHP)
        assert_eq!(
            class_hint_from_symbol("Namespace\\Class#method"),
            Some("Class")
        );
        assert_eq!(
            class_hint_from_symbol("GenericUtils#getIp"),
            Some("GenericUtils")
        );
        // Path-qualified SCIP symbol: hint is the last path segment of the scope.
        assert_eq!(class_hint_from_symbol("src/foo/Bar#baz()."), Some("Bar"));
        // '::' separator (Rust / Go)
        assert_eq!(
            class_hint_from_symbol("crate::module::Class::method"),
            Some("Class")
        );
        assert_eq!(
            class_hint_from_symbol("MyModule::authenticate"),
            Some("MyModule")
        );
        // '.' separator (Java / Python / JS)
        assert_eq!(
            class_hint_from_symbol("com.example.Foo.method"),
            Some("Foo")
        );
        assert_eq!(
            class_hint_from_symbol("module.MyClass.do_thing"),
            Some("MyClass")
        );
        // No method separator → None
        assert_eq!(class_hint_from_symbol("bare_function"), None);
        assert_eq!(class_hint_from_symbol("Namespace\\Class"), None);
    }
}
