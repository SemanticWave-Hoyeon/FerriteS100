//! Device-wide GPU utilization. Not this application's share or GPU pass timing.
//! macOS driver statistics are optional and are never synthesized on failure.

pub fn sample(adapter: &str) -> Option<f32> {
    #[cfg(target_os = "macos")]
    {
        macos::sample(adapter)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = adapter;
        None
    }
}

#[cfg(any(target_os = "macos", test))]
fn valid_percent(value: f64) -> Option<f32> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value as f32)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void, CStr};
    type Cf = *const c_void;
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> Cf;
        fn IOServiceGetMatchingServices(port: u32, matching: Cf, iterator: *mut u32) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, allocator: Cf, options: u32) -> Cf;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(value: Cf);
        fn CFGetTypeID(value: Cf) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFStringCreateWithCString(allocator: Cf, text: *const c_char, encoding: u32) -> Cf;
        fn CFStringGetCString(value: Cf, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFDataGetLength(value: Cf) -> isize;
        fn CFDataGetBytePtr(value: Cf) -> *const u8;
        fn CFDictionaryGetValue(dictionary: Cf, key: Cf) -> Cf;
        fn CFNumberGetValue(number: Cf, kind: isize, value: *mut c_void) -> u8;
    }
    const UTF8: u32 = 0x08000100;
    struct OwnedCf(Cf);
    impl OwnedCf {
        fn string(value: &CStr) -> Option<Self> {
            let value =
                unsafe { CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), UTF8) };
            (!value.is_null()).then_some(Self(value))
        }
        fn property(entry: u32, key: &Self) -> Option<Self> {
            let value =
                unsafe { IORegistryEntryCreateCFProperty(entry, key.0, std::ptr::null(), 0) };
            (!value.is_null()).then_some(Self(value))
        }
    }
    impl Drop for OwnedCf {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }
    struct Io(u32);
    impl Drop for Io {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe {
                    IOObjectRelease(self.0);
                }
            }
        }
    }
    // Only exact device model matches are accepted; don't attribute another GPU's
    // load to the selected adapter on multi-GPU machines.
    fn model_matches(model: &OwnedCf, adapter: &str) -> bool {
        let mut buffer = [0u8; 256];
        unsafe {
            if CFGetTypeID(model.0) == CFStringGetTypeID() {
                if CFStringGetCString(
                    model.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as isize,
                    UTF8,
                ) == 0
                {
                    return false;
                }
            } else if CFGetTypeID(model.0) == CFDataGetTypeID() {
                let length = CFDataGetLength(model.0);
                if !(1..256).contains(&length) {
                    return false;
                }
                let bytes = CFDataGetBytePtr(model.0);
                if bytes.is_null() {
                    return false;
                }
                buffer[..length as usize]
                    .copy_from_slice(std::slice::from_raw_parts(bytes, length as usize));
            } else {
                return false;
            }
        }
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        std::str::from_utf8(&buffer[..end]).is_ok_and(|name| name == adapter)
    }
    pub(super) fn sample(adapter: &str) -> Option<f32> {
        if adapter.is_empty() {
            return None;
        }
        let model_key = OwnedCf::string(c"model")?;
        let stats_key = OwnedCf::string(c"PerformanceStatistics")?;
        let usage_key = OwnedCf::string(c"Device Utilization %")?;
        let mut iterator = Io(0);
        let matching = unsafe { IOServiceMatching(c"IOAccelerator".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        // IOKit consumes matching, including on failure. Iterator owns its handle.
        if unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator.0) } != 0 {
            return None;
        }
        let mut result = None;
        let mut matched = false;
        for _ in 0..32 {
            let entry = Io(unsafe { IOIteratorNext(iterator.0) });
            if entry.0 == 0 {
                return result;
            }
            let Some(model) = OwnedCf::property(entry.0, &model_key) else {
                continue;
            };
            if !model_matches(&model, adapter) {
                continue;
            }
            if matched {
                return None;
            } // Ambiguous identical adapters.
            matched = true;
            let Some(stats) = OwnedCf::property(entry.0, &stats_key) else {
                continue;
            };
            unsafe {
                if CFGetTypeID(stats.0) != CFDictionaryGetTypeID() {
                    continue;
                }
                // Dictionary value is borrowed until stats is released.
                let number = CFDictionaryGetValue(stats.0, usage_key.0);
                if number.is_null() || CFGetTypeID(number) != CFNumberGetTypeID() {
                    continue;
                }
                let mut percent = 0f64;
                if CFNumberGetValue(number, 13, (&mut percent as *mut f64).cast()) != 0 {
                    result = super::valid_percent(percent);
                }
            }
        }
        None // Bounded enumeration; don't silently accept an incomplete match set.
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn percentage_is_real_and_bounded() {
        assert_eq!(super::valid_percent(0.), Some(0.));
        assert_eq!(super::valid_percent(13.), Some(13.));
        assert_eq!(super::valid_percent(100.), Some(100.));
        for value in [-1., 100.01, f64::NAN, f64::INFINITY] {
            assert_eq!(super::valid_percent(value), None);
        }
    }
    #[test]
    fn missing_or_wrong_adapter_never_returns_another_devices_load() {
        assert_eq!(super::sample(""), None);
        assert_eq!(super::sample("Ferrite deliberately missing GPU"), None);
    }
}
