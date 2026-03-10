/// Builds a RE2/POSIX-ERE substring pattern that tolerates stripped namespace
/// separators caused by shell quoting.
///
/// When a user types a PHP FQN like `Netatmo\Models\Homes\Home#method` in an
/// *unquoted* shell argument the shell strips the backslashes, yielding
/// `NetatmoModelsHomesHome#method`.  This function generates a pattern that
/// matches **both** forms by:
///
/// 1. Converting any separator character (`\`, `/`, `#`, `.`, `:`) already present
///    in the input into the flexible class `[/\\#.:]*` (zero or more of any
///    separator).
/// 2. Inserting `[/\\#.:]*` at lowercase→uppercase PascalCase word boundaries
///    so that shell-stripped segments (e.g. `NetatmoModels`) also match their
///    separated form (e.g. `Netatmo\Models`).
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
