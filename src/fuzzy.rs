//! Typo-tolerant text matching for searching the songs Pinson already knows
//! (the play history).
//!
//! A query matches when its text appears as-is, or when every word of it
//! starts some word of the fields with a few typos allowed. Case and Latin
//! accents never count ("beyonce" finds "Beyoncé"). A typo is one inserted,
//! deleted, replaced or swapped-adjacent character. Short words must match
//! exactly, so "ab" never matches everything.

/// A prepared search query.
#[derive(Debug, Clone)]
pub struct Matcher {
    /// The whole query, folded, whitespace collapsed.
    phrase: String,
    /// The query's words, folded, with each one's typo budget.
    tokens: Vec<(Vec<char>, usize)>,
}

impl Matcher {
    /// `None` for a blank query (it would match everything).
    pub fn new(query: &str) -> Option<Matcher> {
        let folded = fold(query);
        let words: Vec<&str> = folded.split_whitespace().collect();
        if words.is_empty() {
            return None;
        }
        Some(Matcher {
            phrase: words.join(" "),
            tokens: words
                .iter()
                .map(|w| (w.chars().collect(), typo_budget(w)))
                .collect(),
        })
    }

    /// Whether the query matches the text of `fields` taken together.
    pub fn matches(&self, fields: &[&str]) -> bool {
        let folded = fold(&fields.join(" "));
        let words: Vec<&str> = folded.split_whitespace().collect();
        if words.join(" ").contains(&self.phrase) {
            return true;
        }
        let words: Vec<(&str, Vec<char>)> = words
            .into_iter()
            .map(|w| {
                // "(feat." is still the word "feat."
                let trimmed = w.trim_start_matches(|c: char| !c.is_alphanumeric());
                (w, trimmed.chars().collect())
            })
            .collect();
        self.tokens.iter().all(|(token, budget)| {
            let text: String = token.iter().collect();
            words.iter().any(|(word, chars)| {
                word.contains(&text) || (*budget > 0 && prefix_distance(token, chars) <= *budget)
            })
        })
    }
}

/// Typos allowed in a query word, by how many letters and digits it has.
fn typo_budget(word: &str) -> usize {
    match word.chars().filter(|c| c.is_alphanumeric()).count() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Lowercases and strips the accents of Latin letters.
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'æ' => out.push_str("ae"),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ğ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
            'ł' | 'ľ' => out.push('l'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'œ' => out.push_str("oe"),
            'ř' => out.push('r'),
            'ś' | 'š' | 'ş' => out.push('s'),
            'ß' => out.push_str("ss"),
            'ť' | 'ţ' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            '’' | '‘' => out.push('\''),
            _ => out.push(c),
        }
    }
    out
}

/// The fewest typos that turn `token` into some prefix of `word` (an
/// optimal-string-alignment distance, so a swap of neighbours is one typo).
fn prefix_distance(token: &[char], word: &[char]) -> usize {
    let (n, m) = (token.len(), word.len());
    // d[i][j]: typos between token[..i] and word[..j].
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(token[i - 1] != word[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && token[i - 1] == word[j - 2] && token[i - 2] == word[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n].iter().copied().min().unwrap_or(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(query: &str, title: &str, artists: &str) -> bool {
        Matcher::new(query).unwrap().matches(&[title, artists])
    }

    #[test]
    fn blank_query_is_none() {
        assert!(Matcher::new("").is_none());
        assert!(Matcher::new("   \t").is_none());
    }

    #[test]
    fn exact_substring_still_matches() {
        assert!(m("night", "Midnight City", "M83"));
        assert!(m("m83", "Midnight City", "M83"));
        assert!(m("city m8", "Midnight City", "M83"));
    }

    #[test]
    fn ignores_case_and_accents() {
        assert!(m("BEYONCE", "Halo", "Beyoncé"));
        assert!(m("beyoncé", "Halo", "Beyonce"));
        assert!(m("coeur", "Mon cœur", "Artist"));
        assert!(m("dont", "Don’t Stop Me Now", "Queen"));
    }

    #[test]
    fn words_in_any_order_across_title_and_artists() {
        assert!(m("punk lucky", "Get Lucky", "Daft Punk"));
        assert!(!m("punk nightcall", "Get Lucky", "Daft Punk"));
    }

    #[test]
    fn tolerates_typos() {
        assert!(m("dfat punk", "Get Lucky", "Daft Punk")); // swap
        assert!(m("daf punk", "Get Lucky", "Daft Punk")); // prefix: "daf"
        assert!(m("lucy", "Get Lucky", "Daft Punk")); // prefix of "lucky"
        assert!(m("luckky", "Get Lucky", "Daft Punk")); // extra letter
        assert!(m("kavinksy", "Nightcall", "Kavinsky")); // swap
        assert!(m("stromae", "Alors on danse", "Stromae"));
        assert!(m("stromea", "Alors on danse", "Stromae"));
        assert!(m("rihana", "Umbrella", "Rihanna")); // missing letter
        assert!(m("bohemain rapsody", "Bohemian Rhapsody", "Queen")); // two errors, long word
        assert!(m("feat", "Song (feat. Someone)", "Artist"));
    }

    #[test]
    fn short_words_and_unrelated_words_do_not_match() {
        assert!(!m("ab", "Get Lucky", "Daft Punk"));
        assert!(!m("dog", "Get Lucky", "Daft Punk")); // 3 letters: exact only
        assert!(!m("metallica", "Get Lucky", "Daft Punk"));
        assert!(!m("zzzz", "Midnight City", "M83"));
    }

    #[test]
    fn punctuation_in_the_query_matches_literally() {
        assert!(m("100%", "100% Pure", "Artist"));
        assert!(!m("100%", "100 Pure", "Artist"));
        assert!(!m("a_b", "axb", "Artist"));
    }

    #[test]
    fn prefix_distance_counts_typos() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(prefix_distance(&c("daft"), &c("daft")), 0);
        assert_eq!(prefix_distance(&c("daf"), &c("daft")), 0);
        assert_eq!(prefix_distance(&c("dfat"), &c("daft")), 1);
        assert_eq!(prefix_distance(&c("dabt"), &c("daft")), 1);
        assert_eq!(prefix_distance(&c("dafts"), &c("daft")), 1);
        assert_eq!(prefix_distance(&c("abc"), &c("")), 3);
    }
}
