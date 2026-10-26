//! Rust half of the minimal C library under the vendored Lua interpreter on
//! wasm32-unknown-unknown (`wasm-libc/ferrite_libc.c`): exact floating-point
//! formatting (`printf` `%e %f %g %a`) and parsing (`strtod`), and the heap.
//!
//! Lua turns numbers into strings with `snprintf("%.14g")`/`string.format`
//! and parses them with `strtod`, so these decide whether portrayal output is
//! byte-identical to a native build. Rust's fixed-precision float formatting
//! and its decimal parser are exact (correctly rounded, ties to even), as are
//! the native C libraries; the conversions here only arrange C's layout
//! around them. The pure functions build and are tested on every target
//! against the host C library.

/// C `printf` conversion of `value` for `conv` (one of `eEfFgGaA`) with
/// `precision` (negative: the default) and the `#` flag; sign included,
/// no width padding.
pub fn format_double(conv: u8, precision: i32, alt: bool, value: f64) -> String {
    let upper = conv.is_ascii_uppercase();
    let mut out = String::new();
    if value.is_sign_negative() && !value.is_nan() {
        out.push('-');
    }
    let v = value.abs();
    if !v.is_finite() {
        out.push_str(match (v.is_nan(), upper) {
            (true, false) => "nan",
            (true, true) => "NAN",
            (false, false) => "inf",
            (false, true) => "INF",
        });
        return out;
    }
    let body = match conv.to_ascii_lowercase() {
        b'f' => fixed(v, precision_or(precision, 6), alt),
        b'e' => scientific(v, precision_or(precision, 6), alt),
        b'g' => general(v, precision, alt),
        b'a' => hexadecimal(v, precision, alt),
        _ => String::new(),
    };
    out.push_str(&body);
    if upper {
        out.make_ascii_uppercase();
    }
    out
}

fn precision_or(precision: i32, default: usize) -> usize {
    if precision < 0 {
        default
    } else {
        precision as usize
    }
}

fn fixed(v: f64, precision: usize, alt: bool) -> String {
    let mut s = format!("{v:.precision$}");
    if alt && precision == 0 {
        s.push('.');
    }
    s
}

/// `d.ddde±XX` from Rust's exact `{:.Ne}` (`d.ddde-X`).
fn scientific(v: f64, precision: usize, alt: bool) -> String {
    let (mantissa, exponent) = scientific_parts(v, precision);
    let mut s = mantissa;
    if alt && precision == 0 {
        s.push('.');
    }
    push_exponent(&mut s, 'e', exponent, 2);
    s
}

fn scientific_parts(v: f64, precision: usize) -> (String, i32) {
    let s = format!("{v:.precision$e}");
    let (mantissa, exponent) = s.split_once('e').expect("Rust {:e} has an exponent");
    (
        mantissa.to_owned(),
        exponent.parse().expect("integer exponent"),
    )
}

fn push_exponent(s: &mut String, marker: char, exponent: i32, min_digits: usize) {
    s.push(marker);
    s.push(if exponent < 0 { '-' } else { '+' });
    let digits = exponent.unsigned_abs().to_string();
    for _ in digits.len()..min_digits {
        s.push('0');
    }
    s.push_str(&digits);
}

/// C99 `%g`: precision P (0 means 1), the exponent X of the `%e` form;
/// `%f` with precision P-1-X when P > X >= -4, else `%e` with P-1; trailing
/// zeros (and a bare point) removed unless `#`.
fn general(v: f64, precision: i32, alt: bool) -> String {
    let p = match precision {
        p if p < 0 => 6,
        0 => 1,
        p => p as usize,
    };
    let (_, x) = scientific_parts(v, p - 1);
    let mut s = if (p as i64) > x as i64 && x >= -4 {
        fixed(v, (p as i64 - 1 - x as i64) as usize, alt)
    } else {
        scientific(v, p - 1, alt)
    };
    if !alt {
        let (number, exponent) = match s.find('e') {
            Some(at) => (s[..at].to_owned(), s[at..].to_owned()),
            None => (s.clone(), String::new()),
        };
        let number = if number.contains('.') {
            number
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        } else {
            number
        };
        s = number + &exponent;
    }
    s
}

/// C99 `%a`: `0x1.hhhp±d` (normalized, also for subnormals), shortest exact
/// digits by default, rounded to nearest-even with a precision.
fn hexadecimal(v: f64, precision: i32, alt: bool) -> String {
    if v == 0.0 {
        let mut s = String::from("0x0");
        if precision > 0 {
            s.push('.');
            s.extend(std::iter::repeat_n('0', precision as usize));
        } else if alt {
            s.push('.');
        }
        s.push_str("p+0");
        return s;
    }
    let bits = v.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    let mut fraction = bits & ((1u64 << 52) - 1);
    let mut exponent = raw_exponent - 1023;
    if raw_exponent == 0 {
        // Subnormal: normalize so the leading 1 is implicit.
        let shift = fraction.leading_zeros() - 11;
        fraction = (fraction << shift) & ((1u64 << 52) - 1);
        exponent = -1022 - shift as i32;
    }
    let mut lead = 1u64;
    let mut digits = 13usize;
    if precision >= 0 && (precision as usize) < 13 {
        let drop = 4 * (13 - precision as usize);
        let kept = fraction >> drop;
        let rest = fraction & ((1u64 << drop) - 1);
        let half = 1u64 << (drop - 1);
        let mut rounded = kept;
        if rest > half || (rest == half && kept & 1 == 1) {
            rounded += 1;
        }
        digits = precision as usize;
        if digits == 0 {
            lead += rounded;
            fraction = 0;
        } else if rounded >> (4 * digits) != 0 {
            lead += 1;
            fraction = 0;
        } else {
            fraction = rounded << drop;
        }
    }
    let mut hex = format!("{fraction:013x}");
    hex.truncate(digits.min(13));
    if precision < 0 {
        while hex.ends_with('0') {
            hex.pop();
        }
    } else {
        for _ in hex.len()..precision as usize {
            hex.push('0');
        }
    }
    let mut s = format!("0x{lead}");
    if !hex.is_empty() || alt {
        s.push('.');
    }
    s.push_str(&hex);
    push_exponent(&mut s, 'p', exponent, 1);
    s
}

fn is_space(c: u8) -> bool {
    c == b' ' || (b'\t'..=b'\r').contains(&c)
}

/// C99 `strtod` of the longest valid prefix: the value and the number of
/// bytes consumed (0 when there is no number).
pub fn parse_double(s: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negative = s[i] == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let starts =
        |word: &[u8]| rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word);
    let signed = |v: f64| if negative { -v } else { v };
    if starts(b"infinity") {
        return (signed(f64::INFINITY), i + 8);
    }
    if starts(b"inf") {
        return (signed(f64::INFINITY), i + 3);
    }
    if starts(b"nan") {
        let mut end = i + 3;
        if s.get(end) == Some(&b'(') {
            let mut j = end + 1;
            while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'_') {
                j += 1;
            }
            if s.get(j) == Some(&b')') {
                end = j + 1;
            }
        }
        return (signed(f64::NAN), end);
    }
    if rest.len() > 2 && rest[0] == b'0' && (rest[1] | 0x20) == b'x' {
        if let Some((value, used)) = parse_hex(&rest[2..]) {
            return (signed(value), i + 2 + used);
        }
        // "0x" without hex digits: the number is the "0".
        return (signed(0.0), i + 1);
    }
    let start = i;
    let mut integer = Vec::new();
    while i < s.len() && s[i].is_ascii_digit() {
        integer.push(s[i]);
        i += 1;
    }
    let mut fraction = Vec::new();
    if i < s.len() && s[i] == b'.' {
        let mut j = i + 1;
        while j < s.len() && s[j].is_ascii_digit() {
            fraction.push(s[j]);
            j += 1;
        }
        if !integer.is_empty() || !fraction.is_empty() {
            i = j;
        }
    }
    if integer.is_empty() && fraction.is_empty() {
        let _ = start;
        return (0.0, 0);
    }
    let mut exponent: i64 = 0;
    if i < s.len() && (s[i] | 0x20) == b'e' {
        let mut j = i + 1;
        let mut exp_negative = false;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            exp_negative = s[j] == b'-';
            j += 1;
        }
        if j < s.len() && s[j].is_ascii_digit() {
            while j < s.len() && s[j].is_ascii_digit() {
                exponent = (exponent * 10 + (s[j] - b'0') as i64).min(1_000_000);
                j += 1;
            }
            if exp_negative {
                exponent = -exponent;
            }
            i = j;
        }
    }
    let mut text = String::with_capacity(integer.len() + fraction.len() + 12);
    text.push_str(std::str::from_utf8(if integer.is_empty() { b"0" } else { &integer }).unwrap());
    text.push('.');
    text.push_str(std::str::from_utf8(if fraction.is_empty() { b"0" } else { &fraction }).unwrap());
    text.push('e');
    text.push_str(&exponent.to_string());
    let value: f64 = text.parse().expect("normalized decimal literal");
    (signed(value), i)
}

/// Hex significand `h*[.h*]` (at least one digit) with an optional binary
/// exponent `p±d+`, correctly rounded; returns the value and bytes used.
fn parse_hex(s: &[u8]) -> Option<(f64, usize)> {
    let mut i = 0;
    let mut mantissa: u64 = 0;
    let mut sticky = false;
    let mut exponent: i64 = 0;
    let mut any = false;
    let mut seen_point = false;
    while i < s.len() {
        let c = s[i];
        if c == b'.' && !seen_point {
            seen_point = true;
            i += 1;
            continue;
        }
        let Some(d) = (c as char).to_digit(16) else {
            break;
        };
        any = true;
        if mantissa >> 60 == 0 {
            mantissa = mantissa << 4 | d as u64;
            if seen_point {
                exponent -= 4;
            }
        } else {
            sticky |= d != 0;
            if !seen_point {
                exponent += 4;
            }
        }
        i += 1;
    }
    if !any {
        return None;
    }
    if i < s.len() && (s[i] | 0x20) == b'p' {
        let mut j = i + 1;
        let mut negative = false;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            negative = s[j] == b'-';
            j += 1;
        }
        if j < s.len() && s[j].is_ascii_digit() {
            let mut e: i64 = 0;
            while j < s.len() && s[j].is_ascii_digit() {
                e = (e * 10 + (s[j] - b'0') as i64).min(1_000_000);
                j += 1;
            }
            exponent += if negative { -e } else { e };
            i = j;
        }
    }
    if mantissa == 0 {
        return Some((0.0, i));
    }
    // value = mantissa * 2^exponent (+ sticky); round to 53 bits (fewer when
    // subnormal), ties to even.
    let top = 63 - mantissa.leading_zeros() as i64; // bit index of the leading 1
    let unbiased = top + exponent; // exponent of the leading bit
    let min_exponent = -1074i64; // weight of the smallest subnormal bit
    let lowest_kept = (unbiased - 52).max(min_exponent); // weight of the last kept bit
    let drop = lowest_kept - exponent; // low mantissa bits to drop
    let (kept, round_up) = if drop <= 0 {
        (mantissa << (-drop), false)
    } else if drop > 64 {
        (0, false)
    } else if drop == 64 {
        let half = 1u64 << 63;
        (0, mantissa > half || (mantissa == half && sticky))
    } else {
        let kept = mantissa >> drop;
        let rest = mantissa & ((1u64 << drop) - 1);
        let half = 1u64 << (drop - 1);
        (
            kept,
            rest > half || (rest == half && (sticky || kept & 1 == 1)),
        )
    };
    let kept = kept + round_up as u64;
    // kept * 2^lowest_kept: exact unless it overflows (two steps keep each
    // power of two representable).
    let first = (lowest_kept / 2).clamp(-1074, 2048) as i32;
    let second = (lowest_kept - first as i64).clamp(-1074, 2048) as i32;
    let value = kept as f64 * 2f64.powi(first) * 2f64.powi(second);
    Some((value, i))
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod exports {
    //! The C ABI entry points used by `ferrite_libc.c` and the Lua sources.
    use std::alloc::{alloc, alloc_zeroed, dealloc, realloc as grow, Layout};
    use std::ffi::{c_char, c_int, c_void};

    const HEADER: usize = 16;

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
    }

    unsafe fn finish(base: *mut u8, size: usize) -> *mut c_void {
        if base.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `base` has HEADER + size bytes, aligned to HEADER.
        unsafe {
            (base as *mut usize).write(size);
            base.add(HEADER) as *mut c_void
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
        match layout(size) {
            // SAFETY: non-zero layout (HEADER > 0).
            Some(l) => unsafe { finish(alloc(l), size) },
            None => std::ptr::null_mut(),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        match count
            .checked_mul(size)
            .and_then(|n| layout(n).map(|l| (n, l)))
        {
            // SAFETY: non-zero layout.
            Some((n, l)) => unsafe { finish(alloc_zeroed(l), n) },
            None => std::ptr::null_mut(),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: `ptr` came from malloc/calloc/realloc above.
        unsafe {
            let base = (ptr as *mut u8).sub(HEADER);
            let size = (base as *const usize).read();
            dealloc(base, layout(size).expect("allocated layout"));
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            // SAFETY: plain allocation.
            return unsafe { malloc(size) };
        }
        let Some(new_layout) = layout(size) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `ptr` came from these functions; the new size is valid.
        unsafe {
            let base = (ptr as *mut u8).sub(HEADER);
            let old = (base as *const usize).read();
            let moved = grow(
                base,
                layout(old).expect("allocated layout"),
                new_layout.size(),
            );
            finish(moved, size)
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
        // SAFETY: C strings passed by Lua are NUL-terminated.
        let bytes = unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes();
        let (value, used) = super::parse_double(bytes);
        if !end.is_null() {
            // SAFETY: caller-provided out pointer.
            unsafe { *end = s.add(used) as *mut c_char };
        }
        value
    }

    #[no_mangle]
    pub unsafe extern "C" fn ferrite_wasm_format_double(
        out: *mut c_char,
        cap: usize,
        conv: c_char,
        precision: c_int,
        alt: c_int,
        value: f64,
    ) -> usize {
        let text = super::format_double(conv as u8, precision, alt != 0, value);
        let n = text.len().min(cap);
        // SAFETY: `out` has room for `cap` bytes.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out as *mut u8, n) };
        text.len()
    }

    /// Lua's stdout/stderr (print, warnings): discarded; portrayal output
    /// goes through host callbacks.
    #[no_mangle]
    pub extern "C" fn ferrite_wasm_write(_stream: c_int, _data: *const c_char, _len: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_char, c_int, CStr, CString};

    extern "C" {
        fn snprintf(buf: *mut c_char, cap: usize, fmt: *const c_char, ...) -> c_int;
        #[link_name = "strtod"]
        fn host_strtod(s: *const c_char, end: *mut *mut c_char) -> f64;
    }

    fn host_format(format: &str, value: f64) -> String {
        let mut buf = vec![0 as c_char; 512];
        let format = CString::new(format).unwrap();
        // SAFETY: valid buffer and format with one double argument.
        unsafe { snprintf(buf.as_mut_ptr(), buf.len(), format.as_ptr(), value) };
        // SAFETY: snprintf NUL-terminates.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    fn values() -> Vec<f64> {
        let mut values = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            0.25,
            0.125,
            2.5,
            3.5,
            0.05,
            0.15,
            0.35,
            1e15,
            1e16,
            123456789012345678.0,
            1e-5,
            1e-4,
            9.9999e-5,
            99999.95,
            999999.5,
            0.1,
            0.2,
            0.3,
            1.0 / 3.0,
            2.0 / 3.0,
            100.0,
            1e21,
            1e22,
            1e100,
            1e-300,
            5e-324,
            2.2250738585072014e-308,
            f64::MAX,
            f64::MIN_POSITIVE,
            12.5,
            0.0005,
            1234.5,
            1.5e-7,
            6.02214076e23,
            299792458.0,
            -273.15,
            49.21,
            -2.13,
            0.000123456,
            4503599627370496.5,
            9007199254740993.0,
        ];
        // Deterministic pseudo-random doubles over many magnitudes, and
        // decimals with few digits (ties at the printed precision).
        let mut state = 0x9e3779b97f4a7c15u64;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bits = state & !(0x7ffu64 << 52) | (((state >> 52) % 160 + 943) << 52);
            values.push(f64::from_bits(bits));
            values.push((state % 2_000_000) as f64 / 1000.0 - 1000.0);
            values.push((state % 100_000) as f64 / 8.0);
        }
        values
    }

    fn strip_g_zeros(s: &str) -> String {
        let (number, exponent) = s.split_at(s.find(['e', 'E']).unwrap_or(s.len()));
        if !number.contains('.') {
            return s.to_owned();
        }
        number
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
            + exponent
    }

    #[test]
    fn formatting_matches_the_host_c_library() {
        let formats = [
            "%.14g", "%g", "%.3g", "%.0g", "%#g", "%.17g", "%e", "%.0e", "%.3e", "%f", "%.0f",
            "%.1f", "%.2f", "%.3f", "%.10f", "%#.0f", "%E", "%G", "%a", "%A",
        ];
        for value in values() {
            for format in formats {
                let conv = format.as_bytes()[format.len() - 1];
                let alt = format.contains('#');
                let precision = format
                    .split_once('.')
                    .map(|(_, p)| p[..p.len() - 1].parse().unwrap())
                    .unwrap_or(-1);
                let ours = format_double(conv, precision, alt, value);
                let mut host = host_format(format, value);
                if cfg!(target_vendor = "apple") && (conv | 0x20) == b'g' && !alt {
                    // Apple's libc keeps a trailing zero when %g rounds an
                    // exact decimal tie down to it (`%.3g` of 9505 gives
                    // 9.50e+03); C99 and glibc remove it.
                    host = strip_g_zeros(&host);
                }
                assert_eq!(ours, host, "{format} of {value:e}");
            }
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                format_double(b'g', 14, false, value),
                host_format("%.14g", value)
            );
        }
    }

    #[test]
    fn parsing_matches_the_host_c_library() {
        let mut inputs: Vec<String> = [
            "0",
            "1",
            "-1",
            "1.5",
            ".5",
            "5.",
            "1e5",
            "1E+5",
            "1e-5",
            "1e",
            "1e+",
            " 42",
            "\t-0.0",
            "0x10",
            "0x1p4",
            "0X1.8P-1",
            "0x.8",
            "0x",
            "0xg",
            "1.7976931348623157e308",
            "1e309",
            "-1e309",
            "4.9e-324",
            "2e-324",
            "1e-400",
            "inf",
            "-Infinity",
            "nan",
            "abc",
            "",
            "+",
            "-",
            ".",
            "1.2.3",
            "12abc",
            "0x1.fffffffffffff8p1023",
            "0x1p-1074",
            "0x1p-1075",
            "0x1.8p-1074",
            "123456789012345678901234567890",
            "0.1000000000000000055511151231257827",
            "2.2250738585072011e-308",
            "  +7.25e2xyz",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for value in values() {
            inputs.push(format!("{value:e}"));
            inputs.push(host_format("%.17g", value));
            inputs.push(host_format("%.14g", value));
            inputs.push(host_format("%a", value));
        }
        for input in inputs {
            let c = CString::new(input.clone()).unwrap();
            let mut end = std::ptr::null_mut();
            // SAFETY: NUL-terminated input, valid out pointer.
            let expected = unsafe { host_strtod(c.as_ptr(), &mut end) };
            let expected_used = end as usize - c.as_ptr() as usize;
            let (value, used) = parse_double(input.as_bytes());
            assert_eq!(used, expected_used, "consumed bytes of {input:?}");
            assert!(
                value.to_bits() == expected.to_bits() || (value.is_nan() && expected.is_nan()),
                "{input:?}: {value:e} != {expected:e}"
            );
        }
    }
}
