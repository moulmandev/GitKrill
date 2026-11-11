//! The operating system's short date and time formats, for "Follow the
//! system" under Preferences > UI Customization > Date/Time Short Format.
//!
//! Each system is asked once for its patterns and the names it uses for
//! months, weekdays and AM / PM, and the patterns are turned into chrono's
//! format ([`SystemFormat`]), so dates are then printed without asking the
//! system again:
//!
//! - **Windows**: `GetLocaleInfoEx` with the user's locale
//!   (`LOCALE_SSHORTDATE`, `LOCALE_SSHORTTIME`: `M/d/yyyy`, `h:mm tt`), so
//!   changes made in Settings > Time & language > Region count.
//! - **macOS**: CoreFoundation's date formatter for the current locale in
//!   its short styles (`M/d/yy`, `h:mm a`), which follows System Settings.
//! - **Linux**: the `LC_TIME` locale (`LC_ALL`, `LC_TIME`, then `LANG`)
//!   through `nl_langinfo` (`D_FMT`, `T_FMT`), without its seconds.

use chrono::format::{Item, StrftimeItems};

/// The system's formats, in chrono's terms.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemFormat {
    /// The short date (`%-m/%-d/%Y`).
    pub date: String,
    /// The short time of day (`%-I:%M %p`).
    pub time: String,
    /// The names the patterns print; `None` keeps chrono's English ones.
    pub names: Option<Names>,
}

/// Month and weekday names and AM / PM, in the system's language.
#[derive(Clone, Debug, PartialEq)]
pub struct Names {
    pub months: [String; 12],
    pub short_months: [String; 12],
    /// Sunday first.
    pub days: [String; 7],
    pub short_days: [String; 7],
    pub am: String,
    pub pm: String,
}

/// The system's short date and time formats; `None` when they cannot be
/// read (another system, a pattern chrono cannot print).
pub fn read() -> Option<SystemFormat> {
    let format = platform()?;
    (valid(&format.date) && valid(&format.time)).then_some(format)
}

/// Whether chrono prints `spec` without an error.
fn valid(spec: &str) -> bool {
    !spec.trim().is_empty() && StrftimeItems::new(spec).all(|item| !matches!(item, Item::Error))
}

/// Which letters a system's patterns use (Linux has strftime's instead).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub enum Dialect {
    /// `d dd ddd dddd`, `M..MMMM`, `y yy yyyy`, `h H m s`, `tt`.
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows,
    /// Unicode's (ICU, macOS): `d dd`, `E..EEEE`, `M..MMMM`, `y yy`, `a`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Unicode,
}

/// A pattern of letters (`dd.MM.yyyy`, `h:mm a`, text in 'quotes') in
/// chrono's format. Eras and time zones are left out.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn from_letters(pattern: &str, dialect: Dialect) -> String {
    use Dialect::{Unicode, Windows};
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let literal = |out: &mut String, c: char| {
        if c == '%' { out.push_str("%%") } else { out.push(c) }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // 'text' is printed as it is; '' is a quote.
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\'' {
                    if chars.get(j + 1) == Some(&'\'') {
                        out.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                literal(&mut out, chars[j]);
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            literal(&mut out, c);
            i += 1;
            continue;
        }
        let mut n = 1;
        while chars.get(i + n) == Some(&c) {
            n += 1;
        }
        i += n;
        let spec = match (c, n, dialect) {
            ('d', 1, _) => "%-d",
            ('d', 2, _) | ('d', _, Unicode) => "%d",
            ('d', 3, Windows) => "%a",
            ('d', _, Windows) => "%A",
            ('E', 1..=3, Unicode) | ('c' | 'e', 3, Unicode) => "%a",
            ('E', _, Unicode) | ('c' | 'e', _, Unicode) if n >= 4 => "%A",
            ('c' | 'e', _, Unicode) => "%u",
            ('M' | 'L', 1, _) => "%-m",
            ('M' | 'L', 2, _) => "%m",
            ('M' | 'L', 3, _) => "%b",
            ('M' | 'L', _, _) => "%B",
            ('y' | 'Y', 2, _) => "%y",
            ('y', 1, Windows) => "%-y",
            ('y' | 'Y', _, _) => "%Y",
            ('h', 1, _) => "%-I",
            ('h', _, _) => "%I",
            ('K', 1, Unicode) => "%-I",
            ('K', _, Unicode) => "%I",
            ('H', 1, _) => "%-H",
            ('H', _, _) => "%H",
            ('k', 1, Unicode) => "%-H",
            ('k', _, Unicode) => "%H",
            ('m', 1, _) => "%-M",
            ('m', _, _) => "%M",
            ('s', 1, _) => "%-S",
            ('s', _, _) => "%S",
            ('t', _, Windows) | ('a' | 'b' | 'B', _, Unicode) => "%p",
            // Eras, time zones, fractions of a second, quarters, weeks.
            ('g', _, Windows) | ('G' | 'z' | 'Z' | 'v' | 'V' | 'O' | 'X' | 'x' | 'S' | 'Q' | 'q' | 'w' | 'W', _, Unicode) => "",
            _ => {
                for _ in 0..n {
                    literal(&mut out, c);
                }
                continue;
            }
        };
        out.push_str(spec);
    }
    tidy(&out)
}

/// A C library pattern (`%m/%d/%Y`, `%r`) with its shorthands spelled out
/// and the locale's alternative forms (`%Ey`, `%OH`) as the plain ones,
/// which is what chrono reads.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn from_strftime(spec: &str) -> String {
    let mut out = String::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut flag = String::new();
        while let Some(&next) = chars.peek() {
            if matches!(next, '-' | '_' | '0' | '^' | '#' | 'E' | 'O') {
                if !matches!(next, 'E' | 'O' | '^' | '#') {
                    flag.push(next);
                }
                chars.next();
            } else {
                break;
            }
        }
        let Some(letter) = chars.next() else { break };
        match letter {
            'D' => out.push_str("%m/%d/%y"),
            'F' => out.push_str("%Y-%m-%d"),
            'T' => out.push_str("%H:%M:%S"),
            'R' => out.push_str("%H:%M"),
            'r' => out.push_str("%I:%M:%S %p"),
            'x' => out.push_str("%m/%d/%y"),
            'X' => out.push_str("%H:%M:%S"),
            'h' => out.push_str("%b"),
            letter => {
                out.push('%');
                out.push_str(&flag);
                out.push(letter);
            }
        }
    }
    out
}

/// A time pattern without its seconds: the short form.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn without_seconds(spec: &str) -> String {
    let mut out = spec.to_owned();
    for seconds in [":%S", ":%-S", ":%_S", ".%S", ":%0S"] {
        out = out.replace(seconds, "");
    }
    tidy(&out)
}

/// Collapses the spaces left where an era or a zone was taken out.
fn tidy(spec: &str) -> String {
    spec.split(' ').filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(windows)]
fn platform() -> Option<SystemFormat> {
    use windows::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_S1159, LOCALE_S2359, LOCALE_SABBREVDAYNAME1, LOCALE_SABBREVMONTHNAME1, LOCALE_SDAYNAME1,
        LOCALE_SMONTHNAME1, LOCALE_SSHORTDATE, LOCALE_SSHORTTIME,
    };
    use windows::core::PCWSTR;

    // The user's locale (LOCALE_NAME_USER_DEFAULT is a null name).
    let info = |kind: u32| -> Option<String> {
        let mut buffer = [0u16; 128];
        // SAFETY: the buffer is passed with its length.
        let written = unsafe { GetLocaleInfoEx(PCWSTR::null(), kind, Some(&mut buffer)) };
        (written > 1).then(|| String::from_utf16_lossy(&buffer[..written as usize - 1]))
    };
    let date = from_letters(&info(LOCALE_SSHORTDATE)?, Dialect::Windows);
    let time = from_letters(&info(LOCALE_SSHORTTIME)?, Dialect::Windows);
    let list = |first: u32, count: u32| -> Option<Vec<String>> { (0..count).map(|i| info(first + i)).collect() };
    // Windows lists the days from Monday.
    let monday_first = |days: Vec<String>| -> Option<[String; 7]> {
        let mut days = days;
        days.rotate_right(1);
        days.try_into().ok()
    };
    let names = (|| {
        Some(Names {
            months: list(LOCALE_SMONTHNAME1, 12)?.try_into().ok()?,
            short_months: list(LOCALE_SABBREVMONTHNAME1, 12)?.try_into().ok()?,
            days: monday_first(list(LOCALE_SDAYNAME1, 7)?)?,
            short_days: monday_first(list(LOCALE_SABBREVDAYNAME1, 7)?)?,
            am: info(LOCALE_S1159).unwrap_or_default(),
            pm: info(LOCALE_S2359).unwrap_or_default(),
        })
    })();
    Some(SystemFormat { date, time, names })
}

#[cfg(target_os = "linux")]
fn platform() -> Option<SystemFormat> {
    use std::ffi::CStr;

    // SAFETY: a locale object of our own, read and freed here; the strings
    // nl_langinfo_l returns are copied before it is freed.
    unsafe {
        let mut locale = libc::newlocale(libc::LC_TIME_MASK, c"".as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            locale = libc::newlocale(libc::LC_TIME_MASK, c"C".as_ptr(), std::ptr::null_mut());
        }
        if locale.is_null() {
            return None;
        }
        let item = |item: libc::nl_item| -> String {
            let text = libc::nl_langinfo_l(item, locale);
            if text.is_null() { String::new() } else { CStr::from_ptr(text).to_string_lossy().into_owned() }
        };
        let date = from_strftime(&item(libc::D_FMT));
        let time = without_seconds(&from_strftime(&item(libc::T_FMT)));
        let names = Names {
            months: std::array::from_fn(|i| item(libc::MON_1 + i as libc::nl_item)),
            short_months: std::array::from_fn(|i| item(libc::ABMON_1 + i as libc::nl_item)),
            days: std::array::from_fn(|i| item(libc::DAY_1 + i as libc::nl_item)),
            short_days: std::array::from_fn(|i| item(libc::ABDAY_1 + i as libc::nl_item)),
            am: item(libc::AM_STR),
            pm: item(libc::PM_STR),
        };
        libc::freelocale(locale);
        let names = names.months.iter().all(|name| !name.is_empty()).then_some(names);
        Some(SystemFormat { date, time, names })
    }
}

#[cfg(target_os = "macos")]
fn platform() -> Option<SystemFormat> {
    mac::read()
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform() -> Option<SystemFormat> {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    //! CoreFoundation's date formatter, which follows System Settings.
    use std::ffi::{c_char, c_void};

    use super::{Dialect, Names, SystemFormat, from_letters};

    type CFIndex = isize;
    type CFTypeRef = *const c_void;

    const NO_STYLE: CFIndex = 0;
    const SHORT_STYLE: CFIndex = 1;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    #[allow(non_upper_case_globals)]
    unsafe extern "C" {
        fn CFLocaleCopyCurrent() -> CFTypeRef;
        fn CFDateFormatterCreate(allocator: CFTypeRef, locale: CFTypeRef, date: CFIndex, time: CFIndex) -> CFTypeRef;
        fn CFDateFormatterGetFormat(formatter: CFTypeRef) -> CFTypeRef;
        fn CFDateFormatterCopyProperty(formatter: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef;
        fn CFStringGetLength(string: CFTypeRef) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
        fn CFStringGetCString(string: CFTypeRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> u8;
        fn CFRelease(object: CFTypeRef);
        static kCFDateFormatterMonthSymbols: CFTypeRef;
        static kCFDateFormatterShortMonthSymbols: CFTypeRef;
        static kCFDateFormatterWeekdaySymbols: CFTypeRef;
        static kCFDateFormatterShortWeekdaySymbols: CFTypeRef;
        static kCFDateFormatterAMSymbol: CFTypeRef;
        static kCFDateFormatterPMSymbol: CFTypeRef;
    }

    /// A CFString as a Rust string.
    unsafe fn string(text: CFTypeRef) -> Option<String> {
        if text.is_null() {
            return None;
        }
        unsafe {
            let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(text), UTF8) + 1;
            let mut buffer = vec![0u8; size.max(1) as usize];
            if CFStringGetCString(text, buffer.as_mut_ptr().cast(), size, UTF8) == 0 {
                return None;
            }
            let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
        }
    }

    /// The pattern of a formatter with these styles.
    unsafe fn pattern(locale: CFTypeRef, date: CFIndex, time: CFIndex) -> Option<String> {
        unsafe {
            let formatter = CFDateFormatterCreate(std::ptr::null(), locale, date, time);
            if formatter.is_null() {
                return None;
            }
            // Owned by the formatter.
            let text = string(CFDateFormatterGetFormat(formatter));
            CFRelease(formatter);
            text
        }
    }

    /// The `count` strings of the formatter's property `key`.
    unsafe fn list<const N: usize>(formatter: CFTypeRef, key: CFTypeRef) -> Option<[String; N]> {
        unsafe {
            let array = CFDateFormatterCopyProperty(formatter, key);
            if array.is_null() {
                return None;
            }
            let items: Option<Vec<String>> = if CFArrayGetCount(array) as usize == N {
                (0..N).map(|i| string(CFArrayGetValueAtIndex(array, i as CFIndex))).collect()
            } else {
                None
            };
            CFRelease(array);
            items?.try_into().ok()
        }
    }

    unsafe fn property(formatter: CFTypeRef, key: CFTypeRef) -> Option<String> {
        unsafe {
            let value = CFDateFormatterCopyProperty(formatter, key);
            let text = string(value);
            if !value.is_null() {
                CFRelease(value);
            }
            text
        }
    }

    pub fn read() -> Option<SystemFormat> {
        // SAFETY: CoreFoundation objects created and released here.
        unsafe {
            let locale = CFLocaleCopyCurrent();
            if locale.is_null() {
                return None;
            }
            let date = pattern(locale, SHORT_STYLE, NO_STYLE);
            let time = pattern(locale, NO_STYLE, SHORT_STYLE);
            let formatter = CFDateFormatterCreate(std::ptr::null(), locale, SHORT_STYLE, SHORT_STYLE);
            let names = (!formatter.is_null())
                .then(|| {
                    Some(Names {
                        months: list(formatter, kCFDateFormatterMonthSymbols)?,
                        short_months: list(formatter, kCFDateFormatterShortMonthSymbols)?,
                        days: list(formatter, kCFDateFormatterWeekdaySymbols)?,
                        short_days: list(formatter, kCFDateFormatterShortWeekdaySymbols)?,
                        am: property(formatter, kCFDateFormatterAMSymbol).unwrap_or_default(),
                        pm: property(formatter, kCFDateFormatterPMSymbol).unwrap_or_default(),
                    })
                })
                .flatten();
            if !formatter.is_null() {
                CFRelease(formatter);
            }
            CFRelease(locale);
            Some(SystemFormat {
                date: from_letters(&date?, Dialect::Unicode),
                time: from_letters(&time?, Dialect::Unicode),
                names,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_patterns() {
        let cases = [
            ("M/d/yyyy", "%-m/%-d/%Y"),
            ("dd.MM.yyyy", "%d.%m.%Y"),
            ("yyyy-MM-dd", "%Y-%m-%d"),
            ("dd/MM/yy", "%d/%m/%y"),
            ("ddd, d MMM yyyy", "%a, %-d %b %Y"),
            ("dddd d MMMM", "%A %-d %B"),
            ("h:mm tt", "%-I:%M %p"),
            ("HH:mm", "%H:%M"),
            ("tt h:mm", "%p %-I:%M"),
            ("H' h 'mm", "%-H h %M"),
            ("d 'de' MMMM 'de' yyyy", "%-d de %B de %Y"),
            ("gg yyyy/M/d", "%Y/%-m/%-d"),
            ("yyyy'年'M'月'd'日'", "%Y年%-m月%-d日"),
            ("'100%' d", "100%% %-d"),
            ("''d''", "'%-d'"),
        ];
        for (pattern, spec) in cases {
            assert_eq!(from_letters(pattern, Dialect::Windows), spec, "{pattern}");
            assert!(valid(spec), "{spec}");
        }
    }

    #[test]
    fn unicode_patterns() {
        let cases = [
            ("M/d/yy", "%-m/%-d/%y"),
            ("dd.MM.yy", "%d.%m.%y"),
            ("y/MM/dd", "%Y/%m/%d"),
            ("h:mm a", "%-I:%M %p"),
            ("a h:mm", "%p %-I:%M"),
            ("HH:mm", "%H:%M"),
            ("EEE d MMM", "%a %-d %b"),
            ("EEEE", "%A"),
            ("HH:mm zzz", "%H:%M"),
        ];
        for (pattern, spec) in cases {
            assert_eq!(from_letters(pattern, Dialect::Unicode), spec, "{pattern}");
        }
    }

    #[test]
    fn c_library_patterns() {
        assert_eq!(from_strftime("%m/%d/%Y"), "%m/%d/%Y");
        assert_eq!(from_strftime("%d.%m.%Y"), "%d.%m.%Y");
        assert_eq!(from_strftime("%Ey年%m月%d日"), "%y年%m月%d日");
        assert_eq!(from_strftime("%D"), "%m/%d/%y");
        assert_eq!(without_seconds(&from_strftime("%r")), "%I:%M %p");
        assert_eq!(without_seconds(&from_strftime("%T")), "%H:%M");
        assert_eq!(without_seconds(&from_strftime("%OH時%M分%S秒")), "%H時%M分%S秒");
        assert_eq!(from_strftime("%-d %e %k"), "%-d %e %k");
        for spec in ["%m/%d/%Y", "%I:%M %p", "%e.%-m.", "%k:%M"] {
            assert!(valid(spec), "{spec}");
        }
        assert!(!valid("%Q"));
        assert!(!valid("  "));
    }

    #[test]
    fn the_system_answers_with_something_printable() {
        // Whatever this machine is set to: the formats read, and print.
        if let Some(format) = read() {
            let now = chrono::Local::now();
            assert!(!now.format(&format.date).to_string().is_empty());
            assert!(!now.format(&format.time).to_string().is_empty());
        }
    }
}
