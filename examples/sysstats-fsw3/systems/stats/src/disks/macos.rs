//! Device counters from each `IOBlockStorageDriver`'s `Statistics`
//! property, named by its child `IOMedia`'s BSD name. macOS exposes no SMART
//! health without private interfaces, so `smart` stays empty.
//!
//! The IOKit and CoreFoundation calls allocate on every sample.

use core::ffi::{CStr, c_char, c_void};

use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
};
use core_foundation_sys::number::{
    CFNumberGetTypeID, CFNumberGetValue, CFNumberRef, kCFNumberSInt64Type,
};
use core_foundation_sys::string::{
    CFStringCreateWithCString, CFStringGetCString, CFStringGetTypeID, CFStringRef,
    kCFStringEncodingUTF8,
};

use super::{Counters, DiskSet};
use crate::sample::{Result, SampleError};

type IoObject = libc::mach_port_t;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        main: libc::mach_port_t,
        matching: CFDictionaryRef,
        iter: *mut IoObject,
    ) -> libc::kern_return_t;
    fn IOIteratorNext(iter: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> libc::kern_return_t;
    fn IORegistryEntryGetChildEntry(
        entry: IoObject,
        plane: *const c_char,
        child: *mut IoObject,
    ) -> libc::kern_return_t;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: *const c_void,
        options: u32,
    ) -> CFTypeRef;
}

/// Per-device state; macOS keeps none.
#[derive(Debug, Default)]
pub struct Extra;

/// CoreFoundation keys, created once.
struct Keys {
    statistics: Cf,
    bsd_name: Cf,
    stats: [Cf; 6],
}

pub struct Source {
    keys: Option<Keys>,
}

impl Source {
    pub fn new(_smart: bool) -> Self {
        Self { keys: Keys::new() }
    }

    pub fn read(&mut self, set: &mut DiskSet) -> Result<()> {
        let keys = self.keys.as_ref().ok_or(SampleError::Os {
            call: "CFStringCreateWithCString",
            code: 0,
        })?;
        let iter = Iter::drivers()?;
        let mut name = [0u8; 64];
        while let Some(driver) = iter.next() {
            if let Some((bsd, counters)) = keys.device(&driver, &mut name) {
                set.update(bsd, counters);
            }
        }
        Ok(())
    }
}

impl Keys {
    fn new() -> Option<Self> {
        Some(Self {
            statistics: Cf::string(c"Statistics")?,
            bsd_name: Cf::string(c"BSD Name")?,
            stats: [
                Cf::string(c"Operations (Read)")?,
                Cf::string(c"Operations (Write)")?,
                Cf::string(c"Bytes (Read)")?,
                Cf::string(c"Bytes (Write)")?,
                Cf::string(c"Total Time (Read)")?,
                Cf::string(c"Total Time (Write)")?,
            ],
        })
    }

    /// Reads one driver's BSD name and counters.
    fn device<'n>(&self, driver: &Object, name: &'n mut [u8]) -> Option<(&'n str, Counters)> {
        let stats = driver.property(&self.statistics)?;
        let dict = stats.as_type(unsafe { CFDictionaryGetTypeID() })? as CFDictionaryRef;
        let at = |i: usize| number(dict, &self.stats[i]);
        let counters = Counters {
            reads: at(0),
            writes: at(1),
            read_bytes: at(2),
            write_bytes: at(3),
            read_ns: at(4),
            write_ns: at(5),
            ..Counters::default()
        };
        let media = driver.child()?;
        let bsd = media.property(&self.bsd_name)?;
        Some((string(&bsd, name)?, counters))
    }
}

/// A CoreFoundation object this code owns, released on drop.
struct Cf(CFTypeRef);

impl Cf {
    fn string(text: &CStr) -> Option<Self> {
        // SAFETY: `text` is NUL-terminated UTF-8.
        let s = unsafe {
            CFStringCreateWithCString(kCFAllocatorDefault, text.as_ptr(), kCFStringEncodingUTF8)
        };
        (!s.is_null()).then(|| Self(s.cast()))
    }

    /// The object if it has type `id`.
    fn as_type(&self, id: core_foundation_sys::base::CFTypeID) -> Option<CFTypeRef> {
        // SAFETY: `self.0` is a live CF object.
        (unsafe { CFGetTypeID(self.0) } == id).then_some(self.0)
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: this code holds one reference to `self.0`.
        unsafe { CFRelease(self.0) };
    }
}

/// Reads the `i64` at `key` in `dict`, or zero.
fn number(dict: CFDictionaryRef, key: &Cf) -> u64 {
    let mut value: i64 = 0;
    // SAFETY: `dict` is a live dictionary; the value is borrowed, not owned.
    unsafe {
        let n = CFDictionaryGetValue(dict, key.0) as CFNumberRef;
        if n.is_null() || CFGetTypeID(n.cast()) != CFNumberGetTypeID() {
            return 0;
        }
        CFNumberGetValue(n, kCFNumberSInt64Type, (&raw mut value).cast());
    }
    u64::try_from(value).unwrap_or(0)
}

/// Copies the CF string `s` into `buf`.
fn string<'b>(s: &Cf, buf: &'b mut [u8]) -> Option<&'b str> {
    // SAFETY: `CFStringGetTypeID` has no preconditions.
    let s = s.as_type(unsafe { CFStringGetTypeID() })? as CFStringRef;
    // SAFETY: `buf` holds `buf.len()` writable bytes.
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr().cast(),
            buf.len() as _,
            kCFStringEncodingUTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(buf).ok()?;
    text.to_str().ok()
}

/// An IOKit object, released on drop.
struct Object(IoObject);

impl Object {
    fn property(&self, key: &Cf) -> Option<Cf> {
        // SAFETY: `self.0` is a live registry entry and `key` a CF string.
        let value = unsafe {
            IORegistryEntryCreateCFProperty(
                self.0,
                key.0 as CFStringRef,
                kCFAllocatorDefault.cast(),
                0,
            )
        };
        (!value.is_null()).then(|| Cf(value))
    }

    fn child(&self) -> Option<Object> {
        let mut child = 0;
        // SAFETY: `self.0` is a live registry entry and the plane is NUL-terminated.
        let code =
            unsafe { IORegistryEntryGetChildEntry(self.0, c"IOService".as_ptr(), &mut child) };
        (code == libc::KERN_SUCCESS && child != 0).then(|| Object(child))
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: this code holds one reference to `self.0`.
        unsafe { IOObjectRelease(self.0) };
    }
}

/// An iterator over every `IOBlockStorageDriver`.
struct Iter(Object);

impl Iter {
    fn drivers() -> Result<Self> {
        // SAFETY: the class name is NUL-terminated. `IOServiceGetMatchingServices`
        // takes ownership of `matching`.
        let (code, iter) = unsafe {
            let matching = IOServiceMatching(c"IOBlockStorageDriver".as_ptr());
            let mut iter = 0;
            (IOServiceGetMatchingServices(0, matching, &mut iter), iter)
        };
        if code != libc::KERN_SUCCESS {
            return Err(SampleError::Os {
                call: "IOServiceGetMatchingServices",
                code,
            });
        }
        Ok(Self(Object(iter)))
    }

    fn next(&self) -> Option<Object> {
        // SAFETY: `self.0` is a live iterator.
        let next = unsafe { IOIteratorNext(self.0.0) };
        (next != 0).then(|| Object(next))
    }
}
