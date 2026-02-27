use protobuf::Message;
use std::env;
use std::fs;

fn main() {
    let path = env::args()
        .nth(1)
        .expect("Usage: dump_scip <path-to-index.scip>");
    let bytes = fs::read(&path).unwrap();
    let index = scip::types::Index::parse_from_bytes(&bytes).unwrap();

    let filter = env::args().nth(2).unwrap_or_default();

    // Stats mode: show aggregate counts
    if filter == "--stats" {
        let mut total_occ: u64 = 0;
        let mut definitions: u64 = 0;
        let mut locals: u64 = 0;
        let mut empty_range: u64 = 0;
        let mut references: u64 = 0;
        let mut empty_symbol: u64 = 0;

        for doc in &index.documents {
            for occ in &doc.occurrences {
                total_occ += 1;
                if occ.symbol_roles & 1 != 0 {
                    definitions += 1;
                } else if occ.symbol.starts_with("local ") {
                    locals += 1;
                } else if occ.range.is_empty() {
                    empty_range += 1;
                } else if occ.symbol.is_empty() {
                    empty_symbol += 1;
                } else {
                    references += 1;
                }
            }
        }

        println!("Documents:     {}", index.documents.len());
        println!("Total occ:     {}", total_occ);
        println!(
            "  Definitions: {} ({:.1}%)",
            definitions,
            definitions as f64 / total_occ as f64 * 100.0
        );
        println!(
            "  Locals:      {} ({:.1}%)",
            locals,
            locals as f64 / total_occ as f64 * 100.0
        );
        println!(
            "  Empty range: {} ({:.1}%)",
            empty_range,
            empty_range as f64 / total_occ as f64 * 100.0
        );
        println!(
            "  Empty symbol:{} ({:.1}%)",
            empty_symbol,
            empty_symbol as f64 / total_occ as f64 * 100.0
        );
        println!(
            "  References:  {} ({:.1}%)",
            references,
            references as f64 / total_occ as f64 * 100.0
        );
        return;
    }

    for doc in &index.documents {
        if !filter.is_empty() && !doc.relative_path.contains(&filter) {
            continue;
        }

        println!("=== {} (lang: {}) ===", doc.relative_path, doc.language);

        if !doc.symbols.is_empty() {
            println!(
                "\n  --- Symbol Information ({} total) ---",
                doc.symbols.len()
            );
            for si in doc.symbols.iter().take(200) {
                println!("    symbol: {}", si.symbol);
                println!("      kind: {:?}", si.kind);
            }
        }

        println!(
            "\n  --- Occurrences ({} total, showing first 200) ---",
            doc.occurrences.len()
        );
        for (i, occ) in doc.occurrences.iter().enumerate().take(200) {
            let role_str = if occ.symbol_roles & 1 != 0 {
                "Definition"
            } else if occ.symbol_roles & 2 != 0 {
                "Import"
            } else {
                "Reference"
            };
            println!(
                "    [{}] {} roles={} range={:?} symbol={}",
                i, role_str, occ.symbol_roles, occ.range, occ.symbol
            );
        }
        println!();

        if filter.is_empty() {
            // Only dump first file when no filter
            break;
        }
    }
}
