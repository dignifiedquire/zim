/// Namespaces seperate different types of directory entries - which might have the same title -
/// stored in the ZIM File Format.
///
/// The "new" namespace scheme (format version 6.1 and later) only uses [`Namespace::UserContent`],
/// [`Namespace::Metadata`], [`Namespace::CategoriesArticle`] (well known entries) and
/// [`Namespace::FulltextIndex`] (search indexes). The remaining variants are from the "old" scheme
/// and are still found in version 6.0 and earlier archives.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Namespace {
    Layout,
    Articles,
    ArticleMetaData,
    UserContent,
    ImagesFile,
    ImagesText,
    Metadata,
    CategoriesText,
    CategoriesArticleList,
    CategoriesArticle,
    FulltextIndex,
    /// A namespace byte that is not defined by either scheme.
    ///
    /// The spec does not reserve the namespace byte, and libzim does not validate it, so an
    /// unknown value must not abort parsing.
    Other(u8),
}

impl Namespace {
    /// The raw byte as stored in the directory entry.
    pub fn as_byte(self) -> u8 {
        use Namespace::*;
        match self {
            Layout => b'-',
            Articles => b'A',
            ArticleMetaData => b'B',
            UserContent => b'C',
            ImagesFile => b'I',
            ImagesText => b'J',
            Metadata => b'M',
            CategoriesText => b'U',
            CategoriesArticleList => b'V',
            CategoriesArticle => b'W',
            FulltextIndex => b'X',
            Other(raw) => raw,
        }
    }
}

impl From<u8> for Namespace {
    fn from(value: u8) -> Self {
        use Namespace::*;
        match value {
            b'-' => Layout,
            b'A' => Articles,
            b'B' => ArticleMetaData,
            b'C' => UserContent,
            b'I' => ImagesFile,
            b'J' => ImagesText,
            b'M' => Metadata,
            b'U' => CategoriesText,
            b'V' => CategoriesArticleList,
            b'W' => CategoriesArticle,
            b'X' => FulltextIndex,
            raw => Other(raw),
        }
    }
}

impl From<Namespace> for u8 {
    fn from(ns: Namespace) -> u8 {
        ns.as_byte()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_every_byte() {
        // An unknown namespace must never abort parsing, and must survive a roundtrip so that
        // path ordering (which compares the raw byte) stays correct.
        for raw in 0..=u8::MAX {
            assert_eq!(Namespace::from(raw).as_byte(), raw);
        }
    }

    #[test]
    fn new_scheme_namespaces_are_known() {
        for (raw, expected) in [
            (b'C', Namespace::UserContent),
            (b'M', Namespace::Metadata),
            (b'W', Namespace::CategoriesArticle),
            (b'X', Namespace::FulltextIndex),
        ] {
            assert_eq!(Namespace::from(raw), expected);
        }
    }
}
