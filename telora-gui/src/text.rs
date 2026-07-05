//! Post-processing of the raw transcription returned by the speech-to-text
//! engine before it reaches the user's clipboard (either typed or copied).
//!
//! Whisper occasionally produces output that is not quite what the user
//! said:
//! - The first word sometimes comes back entirely in lower case
//!   (e.g. "hola mundo" instead of "Hola mundo"), particularly after
//!   cold starts or noisy input.
//! - Runs of two or more spaces (or mixed whitespace) sometimes appear
//!   between words.
//! - The transcription is often missing a closing period at the end.
//!
//! [`clean_transcription`] applies a small, conservative normalization
//! that fixes these issues without altering the rest of the sentence.

/// Trim surrounding whitespace, collapse internal whitespace runs to a
/// single space, and title-case the first word (its first letter
/// upper-cased, the remaining letters of that word lower-cased).
///
/// Subsequent words are left exactly as the engine produced them, so
/// casing elsewhere (proper nouns, acronyms, etc.) is preserved.
///
/// The first-word rule is skipped when the first word does not begin
/// with a letter (numbers, punctuation, URLs, etc.), to avoid mangling
/// content that should be left alone.
///
/// If the result does not already end with sentence-ending punctuation
/// (`.`, `?`, `!`, or the ellipsis `…`), a final `.` is appended so
/// the cleaned text reads as a complete sentence.
pub fn clean_transcription(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut chars = collapsed.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return String::new(),
    };

    let mut result = if first.is_alphabetic() {
        let first_upper: String = first.to_uppercase().collect();
        let mut result = String::with_capacity(collapsed.len());
        result.push_str(&first_upper);

        let mut in_first_word = true;
        for c in chars {
            if in_first_word {
                if c.is_whitespace() {
                    in_first_word = false;
                    result.push(c);
                } else {
                    for lc in c.to_lowercase() {
                        result.push(lc);
                    }
                }
            } else {
                result.push(c);
            }
        }

        result
    } else {
        collapsed
    };

    if let Some(last) = result.chars().last()
        && !matches!(last, '.' | '?' | '!' | '…')
    {
        result.push('.');
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty_string() {
        assert_eq!(clean_transcription(""), "");
        assert_eq!(clean_transcription("   "), "");
        assert_eq!(clean_transcription("\t\n"), "");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(clean_transcription("  hola  "), "Hola.");
        assert_eq!(clean_transcription("\nHola mundo\t"), "Hola mundo.");
    }

    #[test]
    fn collapses_multiple_internal_spaces() {
        assert_eq!(clean_transcription("Hola  mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("Hola   mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("a  b  c"), "A b c.");
    }

    #[test]
    fn collapses_mixed_whitespace_runs() {
        assert_eq!(clean_transcription("Hola\tmundo"), "Hola mundo.");
        assert_eq!(clean_transcription("Hola \t mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("Hola\n\nmundo"), "Hola mundo.");
    }

    #[test]
    fn capitalizes_first_letter_of_lowercase_first_word() {
        assert_eq!(clean_transcription("hola mundo"), "Hola mundo.");
    }

    #[test]
    fn lowercases_remaining_letters_of_first_word() {
        assert_eq!(clean_transcription("HOLA mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("hOLa mundo"), "Hola mundo.");
    }

    #[test]
    fn leaves_subsequent_words_untouched() {
        assert_eq!(clean_transcription("hola MUNDO"), "Hola MUNDO.");
        assert_eq!(clean_transcription("hola Mundo"), "Hola Mundo.");
    }

    #[test]
    fn handles_single_word() {
        assert_eq!(clean_transcription("hola"), "Hola.");
        assert_eq!(clean_transcription("HOLA"), "Hola.");
    }

    #[test]
    fn does_not_touch_first_word_starting_with_non_letter() {
        // First-word title-casing is skipped only when the first
        // character is not a letter. Punctuation- or digit-led words
        // are left exactly as the engine produced them.
        assert_eq!(clean_transcription("1. hola"), "1. hola.");
        assert_eq!(clean_transcription("¿qué tal?"), "¿qué tal?");
    }

    #[test]
    fn preserves_apostrophes_within_first_word() {
        assert_eq!(clean_transcription("don't stop"), "Don't stop.");
        assert_eq!(clean_transcription("DON'T stop"), "Don't stop.");
    }

    #[test]
    fn handles_unicode_letters() {
        assert_eq!(clean_transcription("ñandú"), "Ñandú.");
        assert_eq!(clean_transcription("ÁRBOL"), "Árbol.");
    }

    #[test]
    fn combines_all_normalizations() {
        assert_eq!(
            clean_transcription("  hola   MUNDO   cruel  "),
            "Hola MUNDO cruel."
        );
    }

    #[test]
    fn appends_period_when_missing() {
        assert_eq!(clean_transcription("Hola mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("hola mundo"), "Hola mundo.");
        assert_eq!(clean_transcription("Hola"), "Hola.");
        assert_eq!(clean_transcription("Hola, mundo"), "Hola, mundo.");
    }

    #[test]
    fn preserves_existing_period() {
        assert_eq!(clean_transcription("Hola mundo."), "Hola mundo.");
        assert_eq!(clean_transcription("hola mundo."), "Hola mundo.");
        assert_eq!(clean_transcription("Hola."), "Hola.");
        assert_eq!(clean_transcription("Hola..."), "Hola...");
    }

    #[test]
    fn preserves_existing_question_mark() {
        assert_eq!(clean_transcription("¿Qué tal?"), "¿Qué tal?");
        assert_eq!(clean_transcription("qué tal?"), "Qué tal?");
        assert_eq!(clean_transcription("Hola mundo?"), "Hola mundo?");
    }

    #[test]
    fn preserves_existing_exclamation() {
        assert_eq!(clean_transcription("¡Hola!"), "¡Hola!");
        assert_eq!(clean_transcription("Hola mundo!"), "Hola mundo!");
        assert_eq!(clean_transcription("hola!"), "Hola!");
    }

    #[test]
    fn preserves_existing_ellipsis() {
        assert_eq!(clean_transcription("Hola mundo…"), "Hola mundo…");
        assert_eq!(clean_transcription("hola…"), "Hola…");
    }
}
