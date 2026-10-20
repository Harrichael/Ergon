/// Slug characters: alphanumerics minus look-alikes (`0 O o 1 l I 5 S s 2 Z z
/// 8 B`) and capitals that differ from their lowercase only by size
/// (`C K P U V W X`).
pub const ALPHABET: &[u8] = b"34679ADEFGHJLMNQRTYabcdefghijkmnpqrtuvwxy";

pub const LEN: usize = 4;

/// The root task's fixed slug. It can't collide with a random one because `o`
/// isn't in the alphabet.
pub const ROOT: &str = "root";

pub fn random() -> String {
    (0..LEN)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

pub fn is_valid(s: &str) -> bool {
    s == ROOT || (s.len() == LEN && s.bytes().all(|b| ALPHABET.contains(&b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet() {
        assert_eq!(ALPHABET.len(), 41);
        for c in "0Oo1lI5Ss2Zz8BCKPUVWX".bytes() {
            assert!(!ALPHABET.contains(&c), "{}", c as char);
        }
        assert!(!is_valid("roo") && !is_valid("r00t") && is_valid(ROOT));
    }

    #[test]
    fn random_slugs_are_valid() {
        for _ in 0..1000 {
            let s = random();
            assert!(is_valid(&s) && s != ROOT, "{s}");
        }
    }
}
