//! What someone likely meant by a name that matches nothing: the names
//! nearest it by edits, where a slip of case or a look-alike character
//! (`0`/`O`, `1`/`l`/`I`, `-`/`_`) costs a third of a typo.

const LOOK_ALIKE: &[&str] = &["0Oo", "1lIi", "5Ss", "2Zz", "8B", "-_"];

/// A typo costs this much; a slip, one.
const TYPO: usize = 3;

/// Up to three of `names` nearest to `typed`, nearest first: those within
/// a typo per four characters (for a slug: a typo and a slip, or a slip in
/// every character), and no further than the nearest one by more than a
/// slip. Two typos in four characters would match half of a big project's
/// slugs by chance.
pub fn nearest<'a>(typed: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let limit = (TYPO + 1).max(typed.chars().count() * TYPO / 4);
    let mut near: Vec<(usize, &str)> = names
        .into_iter()
        .map(|n| (distance(typed, n), n))
        .filter(|(d, _)| *d <= limit)
        .collect();
    near.sort();
    let best = near.first().map_or(0, |(d, _)| *d);
    near.into_iter()
        .take_while(|(d, _)| *d <= best + 1)
        .take(3)
        .map(|(_, n)| n)
        .collect()
}

fn substitution(a: char, b: char) -> usize {
    if a == b {
        0
    } else if a.eq_ignore_ascii_case(&b) || LOOK_ALIKE.iter().any(|g| g.contains(a) && g.contains(b)) {
        1
    } else {
        TYPO
    }
}

/// Edit distance with adjacent swaps (optimal string alignment).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0; b.len() + 1]; a.len() + 1];
    for i in 0..=a.len() {
        d[i][0] = i * TYPO;
    }
    for j in 0..=b.len() {
        d[0][j] = j * TYPO;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let mut best = (d[i - 1][j] + TYPO)
                .min(d[i][j - 1] + TYPO)
                .min(d[i - 1][j - 1] + substitution(a[i - 1], b[j - 1]));
            let same = |x: char, y: char| x.eq_ignore_ascii_case(&y);
            if i > 1 && j > 1 && same(a[i - 1], b[j - 2]) && same(a[i - 2], b[j - 1]) {
                best = best.min(d[i - 2][j - 2] + TYPO);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUGS: [&str; 5] = ["eHvi", "eHvj", "N3ju", "nQmj", "root"];

    #[test]
    fn a_slip_of_case_beats_a_typo() {
        assert_eq!(nearest("ehvi", SLUGS), ["eHvi"]);
        assert_eq!(nearest("EHVI", SLUGS), ["eHvi"]);
        assert_eq!(nearest("hEvi", SLUGS), ["eHvi"]);
    }

    #[test]
    fn look_alikes_swaps_and_dropped_letters_are_near() {
        assert_eq!(nearest("N3jv", SLUGS), ["N3ju"]);
        assert_eq!(nearest("nQjm", SLUGS), ["nQmj"]);
        assert_eq!(nearest("r00t", SLUGS), ["root"]);
        assert_eq!(nearest("Q3ju", SLUGS), ["N3ju"]);
        assert_eq!(nearest("auth-finalize", ["auth_finalize", "ergon_choros"]), ["auth_finalize"]);
    }

    #[test]
    fn nothing_close_suggests_nothing() {
        assert!(nearest("zzzz", SLUGS).is_empty());
        assert!(nearest("eHqq", SLUGS).is_empty(), "two typos in four");
    }
}
