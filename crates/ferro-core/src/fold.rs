//! Locale-independent case folding for search.
//!
//! Names and queries are folded the same way and then compared byte-wise. Folding never
//! consults the system locale; Turkish dotless/dotted i is an explicit option.

/// How names are folded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FoldOptions {
    /// Treat `ı` (dotless i) like `i`, so `IŞIK`, `ışık` and `isik`-style spellings
    /// with dotted `i` all match each other. `I` and `İ` always fold to `i`.
    pub turkish_i: bool,
}

/// Appends the folded form of `s` to `out`.
pub fn fold_into(s: &str, options: FoldOptions, out: &mut Vec<u8>) {
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match c {
            // Default lowercase of İ is "i̇" (i + combining dot); fold to plain i instead.
            'İ' => out.push(b'i'),
            'ı' if options.turkish_i => out.push(b'i'),
            c if c.is_ascii() => out.push(c.to_ascii_lowercase() as u8),
            c => {
                for lower in c.to_lowercase() {
                    out.extend_from_slice(lower.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
    }
}

/// Folded form of `s` as a new buffer.
pub fn fold(s: &str, options: FoldOptions) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    fold_into(s, options, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> String {
        String::from_utf8(fold(s, FoldOptions::default())).unwrap()
    }

    fn tr(s: &str) -> String {
        String::from_utf8(fold(s, FoldOptions { turkish_i: true })).unwrap()
    }

    #[test]
    fn ascii_and_unicode() {
        assert_eq!(f("Report.PDF"), "report.pdf");
        assert_eq!(f("ÇALIŞMA Ğ Ö Ü"), "çalişma ğ ö ü"); // I -> i without the Turkish option
        assert_eq!(f("ΣΟΦΙΑ"), "σοφια");
        assert_eq!(f("İstanbul"), "istanbul");
    }

    #[test]
    fn turkish_option_merges_dotless_i() {
        assert_eq!(f("ışık"), "ışık");
        assert_eq!(tr("ışık"), "işik");
        assert_eq!(tr("IŞIK"), tr("ışık"));
        assert_eq!(tr("İNCİ"), tr("inci"));
    }

    #[test]
    fn independent_of_locale_and_stable() {
        // Folding twice changes nothing.
        for s in ["MiXeD", "ŞeHİr", "Straße", "ǅemal"] {
            let once = f(s);
            assert_eq!(f(&once), once);
        }
    }
}
