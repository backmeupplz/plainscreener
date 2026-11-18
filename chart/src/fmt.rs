//! Number and time formatting shared by the server pages and the wasm chart.
//! Written without `core::fmt` so the wasm stays small.

const SUB: [&str; 10] = ["₀", "₁", "₂", "₃", "₄", "₅", "₆", "₇", "₈", "₉"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn uint(out: &mut String, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &b in &buf[i..] {
        out.push(b as char);
    }
}

fn grouped(out: &mut String, n: u64) {
    if n < 1000 {
        return uint(out, n);
    }
    grouped(out, n / 1000);
    out.push(',');
    let r = n % 1000;
    if r < 100 {
        out.push('0');
    }
    if r < 10 {
        out.push('0');
    }
    uint(out, r);
}

/// Writes `m` with a decimal point `d` digits from the right ("1234", 5 → "0.01234").
fn decimal(out: &mut String, m: u64, d: u32) {
    let p = 10u64.pow(d);
    grouped(out, m / p);
    if d > 0 {
        out.push('.');
        let mut frac = String::new();
        uint(&mut frac, m % p);
        for _ in frac.len()..d as usize {
            out.push('0');
        }
        out.push_str(&frac);
    }
}

/// Fixed number of decimals, used for SVG coordinates and percentages.
pub fn fixed(out: &mut String, x: f64, d: u32) {
    if !x.is_finite() {
        return out.push('0');
    }
    let m = (x.abs() * 10f64.powi(d as i32)).round() as u64;
    if x < 0.0 && m > 0 {
        out.push('-');
    }
    decimal(out, m, d);
}

/// Coordinates: one decimal, no grouping, trailing ".0" dropped.
pub fn coord(out: &mut String, x: f64) {
    let n = (x * 10.0).round() as i64;
    if n < 0 {
        out.push('-');
    }
    let n = n.unsigned_abs();
    uint(out, n / 10);
    if !n.is_multiple_of(10) {
        out.push('.');
        out.push((b'0' + (n % 10) as u8) as char);
    }
}

/// A price with `sig` significant digits: "12,345", "1.234", "0.01234", "0.0₅1234".
pub fn price_sig(out: &mut String, x: f64, sig: i32) {
    if !x.is_finite() {
        return out.push('–');
    }
    if x == 0.0 {
        return out.push('0');
    }
    if x < 0.0 {
        out.push('-');
    }
    let x = x.abs();
    let mut e = x.log10().floor() as i32;
    let mut m = (x / 10f64.powi(e - sig + 1)).round();
    if m >= 10f64.powi(sig) {
        e += 1;
        m = (x / 10f64.powi(e - sig + 1)).round();
    }
    if e >= sig - 1 {
        if x >= 1e18 {
            return compact(out, x);
        }
        grouped(out, x.round() as u64);
    } else if e >= -4 {
        decimal(out, m as u64, (sig - 1 - e) as u32);
    } else {
        out.push_str("0.0");
        let mut zeros = String::new();
        uint(&mut zeros, (-e - 1) as u64);
        for b in zeros.bytes() {
            out.push_str(SUB[(b - b'0') as usize]);
        }
        let mut digits = String::new();
        uint(&mut digits, m as u64);
        out.push_str(digits.trim_end_matches('0'));
    }
}

pub fn price(out: &mut String, x: f64) {
    price_sig(out, x, 4)
}

/// Short amounts: 523, 12.3, 1.23K, 45.6M, 789B, 1.2T.
pub fn compact(out: &mut String, x: f64) {
    if !x.is_finite() {
        return out.push('–');
    }
    if x < 0.0 {
        out.push('-');
    }
    let x = x.abs();
    let (v, unit) = match x {
        x if x >= 1e15 => return out.push_str(">999T"),
        x if x >= 1e12 => (x / 1e12, "T"),
        x if x >= 1e9 => (x / 1e9, "B"),
        x if x >= 1e6 => (x / 1e6, "M"),
        x if x >= 1e3 => (x / 1e3, "K"),
        x if x >= 1.0 || x == 0.0 => (x, ""),
        x => return price_sig(out, x, 2),
    };
    let d = if v >= 100.0 || (unit.is_empty() && v.fract() == 0.0) { 0 } else if v >= 10.0 { 1 } else { 2 };
    fixed(out, v, d);
    out.push_str(unit);
}

pub fn usd(out: &mut String, x: f64) {
    out.push('$');
    price(out, x);
}

pub fn usd_compact(out: &mut String, x: f64) {
    out.push('$');
    compact(out, x);
}

/// A change ratio as a signed percentage: 0.1234 → "+12.34%".
pub fn pct(out: &mut String, r: f64) {
    if !r.is_finite() {
        return out.push('–');
    }
    let p = r * 100.0;
    out.push(if p < 0.0 { '-' } else { '+' });
    let a = p.abs();
    if a >= 10_000.0 {
        compact(out, a);
    } else {
        let d = if a >= 100.0 { 0 } else if a >= 10.0 { 1 } else { 2 };
        fixed(out, a, d);
    }
    out.push('%');
}

/// (year, month 1-12, day) from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn two(out: &mut String, n: u64) {
    if n < 10 {
        out.push('0');
    }
    uint(out, n);
}

/// "18 Nov"
pub fn date(out: &mut String, t: i64) {
    let (_, m, d) = civil(t.div_euclid(86_400));
    uint(out, d as u64);
    out.push(' ');
    out.push_str(MONTHS[m as usize - 1]);
}

/// "18 Nov 2026"
pub fn date_year(out: &mut String, t: i64) {
    date(out, t);
    out.push(' ');
    uint(out, civil(t.div_euclid(86_400)).0 as u64);
}

/// "14:05"
pub fn clock(out: &mut String, t: i64) {
    let s = t.rem_euclid(86_400) as u64;
    two(out, s / 3600);
    out.push(':');
    two(out, s / 60 % 60);
}

/// "18 Nov 14:05"
pub fn date_clock(out: &mut String, t: i64) {
    date(out, t);
    out.push(' ');
    clock(out, t);
}

/// Time since an event: "42s", "5m", "3h", "12d", "4mo", "2y".
pub fn age(out: &mut String, secs: i64) {
    let s = secs.max(0) as u64;
    let (n, unit) = match s {
        s if s < 60 => (s, "s"),
        s if s < 3600 => (s / 60, "m"),
        s if s < 86_400 => (s / 3600, "h"),
        s if s < 30 * 86_400 => (s / 86_400, "d"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "mo"),
        s => (s / (365 * 86_400), "y"),
    };
    uint(out, n);
    out.push_str(unit);
}

/// Calls `f` with a fresh string and returns it, for one-off formatting.
pub fn s(f: impl FnOnce(&mut String)) -> String {
    let mut out = String::new();
    f(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices() {
        let p = |x| s(|o| price(o, x));
        assert_eq!(p(12345.6), "12,346");
        assert_eq!(p(1234.5), "1,235");
        assert_eq!(p(1.23456), "1.235");
        assert_eq!(p(0.0123456), "0.01235");
        assert_eq!(p(0.00012345), "0.0001235");
        assert_eq!(p(0.0000012345), "0.0₅1235");
        assert_eq!(p(0.000000000012), "0.0₁₀12");
        assert_eq!(p(9.99996), "10.00");
        assert_eq!(p(0.0), "0");
        assert_eq!(p(f64::NAN), "–");
    }

    #[test]
    fn amounts() {
        let c = |x| s(|o| compact(o, x));
        assert_eq!(c(523.4), "523");
        assert_eq!(c(12.34), "12.3");
        assert_eq!(c(1234.0), "1.23K");
        assert_eq!(c(45_600_000.0), "45.6M");
        assert_eq!(c(789e9), "789B");
        assert_eq!(c(1.8e31), ">999T");
        assert_eq!(c(0.0), "0");
        assert_eq!(s(|o| pct(o, 0.1234)), "+12.3%");
        assert_eq!(s(|o| pct(o, 0.01234)), "+1.23%");
        assert_eq!(s(|o| pct(o, -0.5)), "-50.0%");
        assert_eq!(s(|o| pct(o, 250.0)), "+25.0K%");
        assert_eq!(s(|o| coord(o, 12.34)), "12.3");
        assert_eq!(s(|o| coord(o, -3.0)), "-3");
    }

    #[test]
    fn times() {
        let t = 1_794_992_700; // 2026-11-18 09:05 UTC
        assert_eq!(s(|o| date_clock(o, t)), "18 Nov 09:05");
        assert_eq!(s(|o| date_year(o, 0)), "1 Jan 1970");
        assert_eq!(s(|o| age(o, 7200)), "2h");
    }
}
