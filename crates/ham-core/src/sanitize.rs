//! Escape sanitizer for agent-originated text.
//!
//! Strips ESC sequences (CSI, OSC including OSC 52 and OSC 8, DCS, SOS, PM, APC and two-byte
//! escapes), the 8-bit CSI, OSC and DCS forms with their parameters, every other C1 and C0 control
//! except `\n` and `\t`, DEL, and the bidirectional overrides and isolates (U+202A-U+202E,
//! U+2066-U+2069) that reorder what a terminal shows. An unterminated sequence is dropped to the
//! end of the input. [`StreamSanitizer`] gives the same text for output that arrives in frames,
//! holding a sequence or a UTF-8 character that a frame boundary splits.
//!
//! [`display_text`] and [`display_line`] are the sanitizer of the transcript: the same stripping, but the bidirectional
//! controls and the zero-width and other invisible format characters are shown as `<U+XXXX>` markers instead of vanishing, a line
//! or paragraph separator becomes a newline, and a run of combining marks is cut. [`sanitize`] and [`sanitize_line`] keep deleting
//! them (titles, single-line cells).

/// String-type sequences end at ST (`ESC \` or U+009C), and an OSC string (`bel_ends`) also at BEL; a BEL inside a DCS, SOS, PM or APC
/// string is payload. `None` when the input ends first.
fn skip_string(chars: &[char], mut i: usize, bel_ends: bool) -> Option<usize> {
    while i < chars.len() {
        match chars[i] {
            '\u{7}' if bel_ends => return Some(i + 1),
            '\u{9c}' => return Some(i + 1),
            '\u{1b}' if chars.get(i + 1) == Some(&'\\') => return Some(i + 2),
            _ => i += 1,
        }
    }
    None
}

/// An escape sequence of intermediate bytes (0x20..=0x2F) and one final byte (0x30..=0x7E): `ESC ( B`, `ESC $ ( B`, `ESC # 8`, `ESC SP F`.
/// `None` when the input ends inside it; a byte that is no final byte ends it without being consumed.
fn skip_intermediates(chars: &[char], mut i: usize) -> Option<usize> {
    while i < chars.len() && (0x20..=0x2f).contains(&(chars[i] as u32)) {
        i += 1;
    }
    match chars.get(i) {
        None => None,
        Some(c) if (0x30..=0x7e).contains(&(*c as u32)) => Some(i + 1),
        Some(_) => Some(i),
    }
}

/// CSI ends at the first byte in 0x40..=0x7E after parameter and intermediate bytes; `None` when
/// the input ends first.
fn skip_csi(chars: &[char], mut i: usize) -> Option<usize> {
    while i < chars.len() {
        let c = chars[i] as u32;
        i += 1;
        if (0x40..=0x7e).contains(&c) {
            return Some(i);
        }
        if !(0x20..=0x3f).contains(&c) {
            // Not a CSI byte: the sequence is malformed; drop only what was consumed.
            return Some(i - 1);
        }
    }
    None
}

fn is_dropped_control(c: char) -> bool {
    let n = c as u32;
    (n < 0x20 && c != '\n' && c != '\t') || n == 0x7f || (0x80..=0x9f).contains(&n)
}

/// Bidirectional embeddings, overrides and isolates (Trojan Source, CVE-2021-42574).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// The most combining marks kept after one base character; a longer run (a "zalgo" stack) spills over the lines around it.
pub const COMBINING_MAX: usize = 3;

/// Zero-width joiner and non-joiner: part of emoji families, Persian and Indic words, so they stay between characters of the scripts
/// that use them ([`shaping_script`]).
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}')
}

/// The text and emoji presentation selectors; one stays right after a symbol ([`shaping_script`]).
fn is_selector(c: char) -> bool {
    matches!(c, '\u{fe0e}' | '\u{fe0f}')
}

/// The scripts and symbols where the joiners and the emoji selectors shape text: emoji and symbols (not the Braille block), Arabic and
/// Persian, the Indic and South-East Asian scripts. Anywhere else (Latin, CJK, Braille) they could only carry hidden data.
fn shaping_script(c: char) -> bool {
    matches!(
        c as u32,
        0x0600..=0x06ff
            | 0x0750..=0x077f
            | 0x08a0..=0x08ff
            | 0xfb50..=0xfdff
            | 0xfe70..=0xfeff
            | 0x0900..=0x0dff
            | 0x0e00..=0x0eff
            | 0x0f00..=0x0fff
            | 0x1000..=0x109f
            | 0x1780..=0x17ff
            | 0x00a9
            | 0x00ae
            | 0x203c
            | 0x2049
            | 0x2122
            | 0x2139
            | 0x2190..=0x27ff
            | 0x2900..=0x2bff
            | 0x3030
            | 0x303d
            | 0x3297
            | 0x3299
            | 0x1f000..=0x1faff
    )
}

/// Characters that draw nothing or change how text is ordered or read, and so are shown as markers: bidirectional embeddings,
/// overrides and isolates, zero-width and directional marks, the word joiner, the byte order mark, the Arabic letter mark, the soft
/// hyphen, the Mongolian vowel separator, invisible math operators, the deprecated format controls, interlinear annotation marks
/// and the Unicode tag characters (an invisible copy of ASCII that models read and people do not).
fn is_marked(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2060}'
            | '\u{feff}'
            | '\u{61c}'
            | '\u{ad}'
            | '\u{180e}'
            | '\u{2061}'..='\u{2064}'
            | '\u{206a}'..='\u{206f}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
            // Fillers and blanks that draw nothing: Hangul fillers, the Braille blank, the combining grapheme joiner, Khmer inherent vowels.
            | '\u{115f}'
            | '\u{1160}'
            | '\u{3164}'
            | '\u{ffa0}'
            | '\u{2800}'
            | '\u{34f}'
            | '\u{17b4}'
            | '\u{17b5}'
            // Variation selectors: Mongolian, the sixteen of the BMP and the supplement; two of them are kept by `finish_display`.
            | '\u{180b}'..='\u{180d}'
            | '\u{180f}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{e0100}'..='\u{e01ef}'
            // Musical and Egyptian hieroglyph format controls.
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{13430}'..='\u{1343f}'
    )
}

/// The visible marker of an invisible character (`<U+202E>`), or `None` for an ordinary one.
pub fn marker(c: char) -> Option<String> {
    is_marked(c).then(|| format!("<U+{:04X}>", c as u32))
}

/// The common combining-mark blocks (diacritics, extended, supplement, for symbols, half marks) and the Hebrew points and Arabic marks.
fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{300}'..='\u{36f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
            | '\u{591}'..='\u{5bd}'
            | '\u{5bf}'
            | '\u{5c1}'..='\u{5c2}'
            | '\u{5c4}'..='\u{5c5}'
            | '\u{5c7}'
            | '\u{610}'..='\u{61a}'
            | '\u{64b}'..='\u{65f}'
            | '\u{670}'
            | '\u{6d6}'..='\u{6dc}'
            | '\u{6df}'..='\u{6e4}'
            | '\u{6e7}'..='\u{6e8}'
            | '\u{6ea}'..='\u{6ed}'
    )
}

/// Sanitizes `chars` into `out`; returns where an unterminated sequence starts, if the input ends
/// inside one (its characters are not written).
fn scan(chars: &[char], out: &mut String) -> Option<usize> {
    scan_mode(chars, out, false)
}

/// [`scan`] with the transcript's rules when `display` is set: invisible characters become markers and U+2028 and U+2029 become
/// newlines (joiners are kept for [`finish_display`] to judge).
fn scan_mode(chars: &[char], out: &mut String, display: bool) -> Option<usize> {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = match c {
            '\u{1b}' => match chars.get(i + 1) {
                None => None,
                Some('[') => skip_csi(chars, i + 2),
                Some(']') => skip_string(chars, i + 2, true),
                Some('P' | 'X' | '^' | '_') => skip_string(chars, i + 2, false),
                // Intermediate bytes then a final byte: character-set designation (ESC ( B, ESC $ ( B), ESC # 8, ESC SP F.
                Some(b) if (0x20..=0x2f).contains(&(*b as u32)) => skip_intermediates(chars, i + 2),
                Some(_) => Some(i + 2),
            },
            '\u{9b}' => skip_csi(chars, i + 1),
            // 8-bit OSC, DCS, SOS, PM and APC carry actions (OSC 52 writes the clipboard): drop their strings.
            // The other C1 code points are dropped one at a time below.
            '\u{9d}' => skip_string(chars, i + 1, true),
            '\u{90}' => skip_string(chars, i + 1, false),
            // 8-bit SOS, PM and APC: a string that ends at an ST goes with its payload; one that does not is dropped as the lone control it
            // is, so a stray byte does not hide the text after it.
            '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(chars, i + 1, false).or(Some(i + 1)),
            _ if display && (is_joiner(c) || is_selector(c)) => {
                out.push(c);
                Some(i + 1)
            }
            _ if display && is_marked(c) => {
                out.push_str(&marker(c).unwrap_or_default());
                Some(i + 1)
            }
            '\u{2028}' | '\u{2029}' if display => {
                out.push('\n');
                Some(i + 1)
            }
            _ if is_dropped_control(c) || is_bidi_control(c) => Some(i + 1),
            _ => {
                out.push(c);
                Some(i + 1)
            }
        };
        match next {
            Some(n) => i = n,
            None => return Some(i),
        }
    }
    None
}

/// Removes escape sequences and control characters; keeps `\n`, `\t` and all printable text.
pub fn sanitize(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    scan(&chars, &mut out);
    out
}

/// [`sanitize`] for single-line cells: newlines, carriage returns and tabs become spaces.
pub fn sanitize_line(input: &str) -> String {
    let flat: String = input
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .collect();
    sanitize(&flat)
}

/// Caps runs of combining marks (a joiner or a selector does not end a run), then decides each joiner (kept between two characters of a
/// shaping script) and each emoji selector (kept right after one), and shows the others as markers.
fn finish_display(text: &str) -> String {
    let mut capped: Vec<char> = Vec::with_capacity(text.len());
    let mut run = 0usize;
    for c in text.chars() {
        if is_combining(c) {
            run += 1;
            if run > COMBINING_MAX {
                continue;
            }
        } else if !is_joiner(c) && !is_selector(c) {
            run = 0;
        }
        capped.push(c);
    }
    let mut out = String::with_capacity(text.len());
    for (i, &c) in capped.iter().enumerate() {
        let keep = if is_joiner(c) {
            i > 0
                && capped.get(i + 1).is_some_and(|n| shaping_script(*n))
                && shaping_script(capped[i - 1])
        } else if is_selector(c) {
            i > 0 && shaping_script(capped[i - 1])
        } else {
            out.push(c);
            continue;
        };
        if keep {
            out.push(c);
        } else {
            out.push_str(&marker(c).unwrap_or_default());
        }
    }
    out
}

/// The transcript's sanitizer: [`sanitize`]'s stripping (every C0 and C1 control except `\n` and `\t`, so carriage return, backspace,
/// bell, DEL and the 8-bit forms go; every escape sequence with its payload, so OSC 8 links and OSC 52 clipboard writes leave only
/// their visible text), but bidirectional controls, zero-width and other invisible format characters and tag characters are shown as
/// `<U+XXXX>` markers ([`marker`]), U+2028 and U+2029 become newlines, and a run of combining marks is cut to [`COMBINING_MAX`].
/// URLs stay plain text. The result is stable: `display_text(display_text(x)) == display_text(x)`.
pub fn display_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    scan_mode(&chars, &mut out, true);
    finish_display(&out)
}

/// [`display_text`] for single-line cells: newlines and tabs become spaces.
pub fn display_line(input: &str) -> String {
    display_text(input)
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .collect()
}

/// Sanitizes a block of agent text for a card: escape sequences and controls removed (as [`sanitize_line`]), line breaks kept,
/// each line wrapped at a space within `max_cols` characters (a longer word is cut at the limit), at most `max_lines` lines, the last
/// one ending with `…` when text was dropped. Blank lines at either end go; blank lines inside stay.
pub fn sanitize_block(input: &str, max_lines: usize, max_cols: usize) -> Vec<String> {
    if max_lines == 0 || max_cols == 0 {
        return Vec::new();
    }
    let mut source: Vec<String> = sanitize(input)
        .split('\n')
        .map(|l| l.replace('\t', " ").trim().to_string())
        .collect();
    while source.last().is_some_and(String::is_empty) {
        source.pop();
    }
    let skip = source.iter().take_while(|l| l.is_empty()).count();
    let mut lines: Vec<String> = source
        .iter()
        .skip(skip)
        .flat_map(|l| wrap_line(l, max_cols))
        .collect();
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            if last.chars().count() >= max_cols {
                *last = last.chars().take(max_cols - 1).collect();
            }
            last.push('\u{2026}');
        }
    }
    lines
}

/// One source line as lines of at most `cols` characters, broken at spaces; a word longer than `cols` is cut at `cols`.
fn wrap_line(line: &str, cols: usize) -> Vec<String> {
    if line.chars().count() <= cols {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in line.split(' ').filter(|w| !w.is_empty()) {
        let w_len = word.chars().count();
        if cur_len > 0 && cur_len + 1 + w_len <= cols {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + w_len;
            continue;
        }
        if cur_len > 0 {
            out.push(std::mem::take(&mut cur));
        }
        // Cut `cols` characters at a time. Looking at the next `cols` characters only keeps the work linear in the word: counting
        // everything that is left after every cut made it grow with the square of the length (Sol's finding on the sanitizer review).
        let mut rest = word;
        while let Some((cut, _)) = rest.char_indices().nth(cols) {
            out.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        cur.push_str(rest);
        cur_len = rest.chars().count();
    }
    if cur_len > 0 {
        out.push(cur);
    }
    out
}

/// [`sanitize`] over a byte stream: `push` returns the text that is safe to show so far; a frame
/// that ends inside an escape sequence or a UTF-8 character keeps that part for the next frame.
/// Invalid UTF-8 becomes U+FFFD. `finish` ends the stream, dropping an unterminated sequence.
#[derive(Debug, Default)]
pub struct StreamSanitizer {
    /// Bytes of an incomplete UTF-8 character.
    bytes: Vec<u8>,
    /// Text from the start of an unterminated sequence.
    held: String,
}

impl StreamSanitizer {
    /// Characters of one unterminated sequence kept between frames; past this only the sequence's
    /// introducer is kept (its body would be dropped anyway).
    pub const MAX_HELD: usize = 4096;

    /// The valid UTF-8 of `self.bytes`; an incomplete character at the end stays in `self.bytes`.
    fn decode(&mut self) -> String {
        let mut out = String::new();
        let mut rest: &[u8] = &self.bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (ok, tail) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(ok).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{fffd}');
                            rest = &tail[n..];
                        }
                        None => {
                            rest = tail;
                            break;
                        }
                    }
                }
            }
        }
        self.bytes = rest.to_vec();
        out
    }

    pub fn push(&mut self, input: &[u8]) -> String {
        self.bytes.extend_from_slice(input);
        let text = self.decode();
        self.held.push_str(&text);
        let chars: Vec<char> = self.held.chars().collect();
        let mut out = String::new();
        match scan(&chars, &mut out) {
            None => self.held.clear(),
            Some(start) => {
                let open = &chars[start..];
                let keep = if open.len() > Self::MAX_HELD {
                    // ESC plus its second character, or the one 8-bit introducer.
                    if open[0] == '\u{1b}' { 2 } else { 1 }
                } else {
                    open.len()
                };
                self.held = open[..keep].iter().collect();
                // The body that is dropped may end on the ESC that begins the ST terminator (ESC \): that ESC is kept, or the backslash
                // of the next frame would find nothing to end and the text after the sequence would be swallowed with it.
                if open.len() > keep && open.last() == Some(&'\u{1b}') {
                    self.held.push('\u{1b}');
                }
            }
        }
        out
    }

    pub fn finish(&mut self) -> String {
        if !self.bytes.is_empty() {
            self.held.push('\u{fffd}');
            self.bytes.clear();
        }
        let out = sanitize(&self.held);
        self.held.clear();
        out
    }
}
