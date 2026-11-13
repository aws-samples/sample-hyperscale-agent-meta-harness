//! Number renderings the TUI shares: grouped digits for a model's window and the compact window of the dashboard's CTX column.

/// `1000000` as `1,000,000`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A window in at most six characters for the dashboard's CTX column: `150k`, `1M`, `1.05M`.
pub fn compact_tokens(n: u32) -> String {
    const M: u32 = 1_000_000;
    if n >= M {
        if n.is_multiple_of(M) {
            return format!("{}M", n / M);
        }
        let s = format!("{:.2}", f64::from(n) / f64::from(M));
        return format!("{}M", s.trim_end_matches('0').trim_end_matches('.'));
    }
    if n >= 1000 && n.is_multiple_of(1000) {
        return format!("{}k", n / 1000);
    }
    n.to_string()
}
