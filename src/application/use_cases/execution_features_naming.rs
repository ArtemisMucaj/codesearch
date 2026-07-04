//! Symbol-name formatting for execution features.
//!
//! Turns a fully-qualified SCIP-style symbol into a short, human-readable label.
//! Kept separate from the BFS/scoring concern in `execution_features.rs` so each
//! file stays focused on one logical concept.

/// Extract the short (human-readable) name from a fully-qualified symbol.
///
/// For member symbols (`Class#member`) the result is `Class.member`; for a
/// bare top-level function it is just the function name. SCIP-style accessor
/// descriptors are translated to their member name: `Class#<get>foo` becomes
/// `Class.foo`, and a `Class#<constructor>` becomes just `Class` (the
/// constructor's readable name is the class it builds).
pub(super) fn short_name(fqn: &str) -> String {
    // Trim a SCIP `path/to/file Package#Method` prefix down to the last segment
    // that carries the class#member (the descriptor never contains a space).
    let fqn = fqn.rsplit(' ').next().unwrap_or(fqn).trim();

    match fqn.rsplit_once('#') {
        // `Class#member` — combine the leaf class name with the member name.
        Some((class, member)) => {
            let class = leaf(class);
            match member_name(member) {
                // Constructor: the class name alone is the clearest label.
                None => class.to_string(),
                Some(member) => format!("{class}.{member}"),
            }
        }
        // No `#`: a bare top-level function (or already-short symbol).
        None => member_name(leaf(fqn)).unwrap_or(leaf(fqn)).to_string(),
    }
}

/// Reduce a possibly path/namespace-qualified identifier to its last segment,
/// splitting on `/`, `::`, and `.`.
fn leaf(s: &str) -> &str {
    let s = s.rsplit_once('/').map(|(_, r)| r).unwrap_or(s);
    let s = s.rsplit_once("::").map(|(_, r)| r).unwrap_or(s);
    s.rsplit_once('.').map(|(_, r)| r).unwrap_or(s)
}

/// Translate a raw member descriptor into a display name.
///
/// Returns `None` for constructors (which have no member name of their own),
/// the accessor target for `<get>`/`<set>` descriptors, and otherwise the
/// member name with any trailing `()` call parens or `<…>` generic parameters
/// stripped.
fn member_name(member: &str) -> Option<&str> {
    let member = member.trim();
    if member == "<constructor>" {
        return None;
    }
    // SCIP accessor descriptors: `<get>foo` / `<set>foo` -> `foo`.
    let member = member
        .strip_prefix("<get>")
        .or_else(|| member.strip_prefix("<set>"))
        .unwrap_or(member);
    // Strip trailing call parens and generic parameters, but only as a suffix
    // so a leading descriptor (already handled above) is never mistaken for one.
    let member = member.split('(').next().unwrap_or(member);
    let member = member.split('<').next().unwrap_or(member);
    let member = member.trim();
    if member.is_empty() {
        None
    } else {
        Some(member)
    }
}

#[cfg(test)]
mod tests {
    use super::short_name;

    #[test]
    fn plain_method_is_qualified_with_class() {
        assert_eq!(
            short_name("RemoteNetwork#getIpMac"),
            "RemoteNetwork.getIpMac"
        );
        assert_eq!(
            short_name("WebsocketChannel#addMembership"),
            "WebsocketChannel.addMembership"
        );
    }

    #[test]
    fn accessor_descriptors_resolve_to_the_member() {
        // Regression: `<get>`/`<set>` prefixes previously collapsed the name to
        // an empty string because the generic-suffix strip split on the leading
        // `<`.
        assert_eq!(
            short_name("NoCertificateAuthority#<get>crypto"),
            "NoCertificateAuthority.crypto"
        );
        assert_eq!(
            short_name("DummyScanner#<get>targetCriteriaProviders"),
            "DummyScanner.targetCriteriaProviders"
        );
        assert_eq!(short_name("Config#<set>timeout"), "Config.timeout");
    }

    #[test]
    fn constructor_is_labelled_by_its_class() {
        assert_eq!(short_name("Producer#<constructor>"), "Producer");
        assert_eq!(
            short_name("SchedulerController#<constructor>"),
            "SchedulerController"
        );
    }

    #[test]
    fn bare_top_level_function_keeps_its_name() {
        assert_eq!(short_name("controllerRouter"), "controllerRouter");
        assert_eq!(short_name("parseCorrelationData"), "parseCorrelationData");
    }

    #[test]
    fn generic_suffixes_are_stripped_but_leading_descriptors_are_not() {
        assert_eq!(short_name("Repo#findAll<T>"), "Repo.findAll");
        assert_eq!(short_name("run()"), "run");
    }

    #[test]
    fn scip_path_prefixed_and_namespaced_symbols_reduce_to_leaf() {
        // SCIP `path Package#Method` shape: only the symbol after the space matters.
        assert_eq!(
            short_name("src/net.ts `net`/RemoteNetwork#getIpMac"),
            "RemoteNetwork.getIpMac"
        );
        // `::`/`.`-qualified class names reduce to their leaf.
        assert_eq!(
            short_name("crate::net::RemoteNetwork#getIpMac"),
            "RemoteNetwork.getIpMac"
        );
    }
}
