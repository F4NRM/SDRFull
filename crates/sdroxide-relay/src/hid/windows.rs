//! HID on Windows: SetupAPI to find the devices, `hid.dll` to talk to them.
//!
//! ⚠️ **Never run against hardware.** This machine is Linux; the code below is
//! written from the Win32 documentation and compiles on the release runner, so
//! a signature error is caught and a wrong buffer offset is not. The
//! `relay` example prints enough of a transcript to settle it from one
//! operator's report — that is what it is there for.
//!
//! `hid.dll` is not among the libraries `windows-sys` links, so the four
//! functions used here are declared and linked explicitly.
//!
//! # Buffer conventions, which differ from every other platform
//!
//! `HidD_SetFeature` and `HidD_GetFeature` both take a buffer whose byte 0 is
//! the report id **in both directions** — unlike Linux, which strips a zero id
//! on the way out and does not put one back on the way in. `HidD_GetFeature`
//! also insists the buffer be exactly the length the device's capabilities
//! declare for a feature report, which for these devices is the report plus the
//! id byte. So the sizes here are `body.len() + 1` throughout, and the answer
//! is read from byte 1.

use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::core::GUID;

use crate::error::{Error, Result};

use super::{HidDev, HidEntry};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HiddAttributes {
    size: u32,
    vendor_id: u16,
    product_id: u16,
    version_number: u16,
}

#[link(name = "hid")]
unsafe extern "system" {
    fn HidD_GetHidGuid(guid: *mut GUID);
    fn HidD_GetAttributes(device: HANDLE, attributes: *mut HiddAttributes) -> i32;
    fn HidD_GetProductString(device: HANDLE, buffer: *mut u16, len: u32) -> i32;
    fn HidD_GetSerialNumberString(device: HANDLE, buffer: *mut u16, len: u32) -> i32;
    fn HidD_SetFeature(device: HANDLE, buffer: *const u8, len: u32) -> i32;
    fn HidD_GetFeature(device: HANDLE, buffer: *mut u8, len: u32) -> i32;
    fn HidD_GetPreparsedData(device: HANDLE, data: *mut isize) -> u8;
    fn HidD_FreePreparsedData(data: isize) -> u8;
    fn HidP_GetCaps(data: isize, caps: *mut HidpCaps) -> i32;
}

/// `HIDP_CAPS` is 64 bytes: five `u16` the switch reading needs and twenty-seven
/// it does not. Only `input_report_len` is read.
#[repr(C)]
struct HidpCaps {
    usage: u16,
    usage_page: u16,
    input_report_len: u16,
    output_report_len: u16,
    feature_report_len: u16,
    rest: [u16; 27],
}

const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;

/// An owned device handle. A `Drop` rather than a bare `HANDLE` because every
/// path out of `enumerate` opens one and most of them throw it away.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// The handle is only ever used from the driver's own thread.
unsafe impl Send for Handle {}

pub struct WinHid {
    handle: Handle,
    path: String,
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn open_path(path: &str) -> Result<Handle> {
    let w = wide(path);
    // Shared: a CM108 is a sound card the rig is also using, and opening it
    // exclusively would take the audio away from whatever is playing it.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h as isize == -1 {
        return Err(Error::opening(path, std::io::Error::last_os_error()));
    }
    Ok(Handle(h))
}

impl HidDev for WinHid {
    fn set_feature(&mut self, report_id: u8, body: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(body.len() + 1);
        buf.push(report_id);
        buf.extend_from_slice(body);
        let ok = unsafe { HidD_SetFeature(self.handle.0, buf.as_ptr(), buf.len() as u32) };
        if ok == 0 {
            return Err(Error::opening(&self.path, std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn get_feature(&mut self, report_id: u8, body: &mut [u8]) -> Result<()> {
        let mut buf = vec![0u8; body.len() + 1];
        buf[0] = report_id;
        let ok = unsafe { HidD_GetFeature(self.handle.0, buf.as_mut_ptr(), buf.len() as u32) };
        if ok == 0 {
            return Err(Error::opening(&self.path, std::io::Error::last_os_error()));
        }
        // Byte 0 comes back as the report id — see the module docs.
        body.copy_from_slice(&buf[1..]);
        Ok(())
    }

    fn write_output(&mut self, report_id: u8, body: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(body.len() + 1);
        buf.push(report_id);
        buf.extend_from_slice(body);
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::opening(&self.path, std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

pub fn open(key: &str) -> Result<Box<dyn HidDev>> {
    let handle = open_path(key)?;
    Ok(Box::new(WinHid { handle, path: key.to_string() }))
}

/// A device opened to read its input reports (issue #636).
///
/// Its own handle, opened for overlapped I/O: `ReadFile` on an ordinary handle
/// blocks until a report arrives, and a reader that cannot be asked to stop
/// would hold the port open for as long as nobody pressed anything. With an
/// event, the wait has a timeout and a stop is a cancelled read.
///
/// `ReadFile` on a HID handle insists on a buffer of exactly the length the
/// device declares for an input report, so that is asked of `hid.dll` at open.
///
/// A keyboard-class device (a pedal that types a key) is owned by the system on
/// Windows and `CreateFileW` refuses it with "access denied"; the error says
/// so.
pub struct WinHidIn {
    handle: Handle,
    event: Handle,
    path: String,
    len: usize,
}

pub fn open_input(key: &str) -> Result<Box<dyn HidDev>> {
    let w = wide(key);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h as isize == -1 {
        return Err(Error::opening(key, std::io::Error::last_os_error()));
    }
    let handle = Handle(h);
    let mut pre = 0isize;
    if unsafe { HidD_GetPreparsedData(handle.0, &mut pre) } == 0 {
        return Err(Error::opening(key, std::io::Error::last_os_error()));
    }
    let mut caps: HidpCaps = unsafe { std::mem::zeroed() };
    let status = unsafe { HidP_GetCaps(pre, &mut caps) };
    unsafe { HidD_FreePreparsedData(pre) };
    if status != HIDP_STATUS_SUCCESS || caps.input_report_len == 0 {
        return Err(Error::Open {
            path: key.to_string(),
            source: std::io::Error::other("the device has no input report to read"),
        });
    }
    // Manual reset: `read_input` resets it itself before every read.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(Error::opening(key, std::io::Error::last_os_error()));
    }
    Ok(Box::new(WinHidIn {
        handle,
        event: Handle(event),
        path: key.to_string(),
        len: usize::from(caps.input_report_len),
    }))
}

impl HidDev for WinHidIn {
    fn set_feature(&mut self, _id: u8, _body: &[u8]) -> Result<()> {
        Err(Error::Unsupported("this handle only reads input reports".into()))
    }
    fn get_feature(&mut self, _id: u8, _body: &mut [u8]) -> Result<()> {
        Err(Error::Unsupported("this handle only reads input reports".into()))
    }
    fn write_output(&mut self, _id: u8, _body: &[u8]) -> Result<()> {
        Err(Error::Unsupported("this handle only reads input reports".into()))
    }

    fn read_input(&mut self, timeout: std::time::Duration) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; self.len];
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        unsafe { ResetEvent(self.event.0) };
        let ok = unsafe {
            ReadFile(self.handle.0, buf.as_mut_ptr(), buf.len() as u32, std::ptr::null_mut(), &mut ov)
        };
        if ok == 0 {
            if unsafe { GetLastError() } != ERROR_IO_PENDING {
                return Err(Error::opening(&self.path, std::io::Error::last_os_error()));
            }
            let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
            if unsafe { WaitForSingleObject(self.event.0, ms) } != WAIT_OBJECT_0 {
                // Nothing yet: cancel, and wait for the cancel so `buf` and `ov`
                // are not written to after this function returns.
                unsafe { CancelIoEx(self.handle.0, &ov) };
                let mut n = 0u32;
                let done = unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, 1) };
                if done == 0 || n == 0 {
                    return Ok(None);
                }
                buf.truncate(n as usize);
                return Ok(Some(strip_id(buf)));
            }
        }
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, 0) } == 0 {
            return Err(Error::opening(&self.path, std::io::Error::last_os_error()));
        }
        buf.truncate(n as usize);
        Ok(Some(strip_id(buf)))
    }
}

/// Windows puts the report id in byte 0 always; Linux and macOS leave it out
/// when it is 0. Dropping a zero id makes the same switch read the same here.
fn strip_id(mut buf: Vec<u8>) -> Vec<u8> {
    if buf.first() == Some(&0) {
        buf.remove(0);
    }
    buf
}

pub fn enumerate(ids: &[(u16, u16)]) -> Vec<HidEntry> {
    let mut out = Vec::new();
    let mut guid: GUID = unsafe { std::mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };

    let set = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    // `HDEVINFO` is an `isize` here, not a pointer: 0 and -1 are both failures.
    if set == 0 || set == -1 {
        return out;
    }

    let mut index = 0u32;
    loop {
        let mut iface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        iface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        let ok =
            unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut iface) };
        if ok == 0 {
            break;
        }
        index += 1;

        // Two calls, as the API demands: one for the size, one for the detail.
        let mut needed = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            )
        };
        if needed == 0 {
            continue;
        }
        // SP_DEVICE_INTERFACE_DETAIL_DATA_W is a `u32` followed by the path,
        // and `cbSize` is the size of the *fixed* part — 8 on 64-bit, where the
        // struct is padded to the alignment of its `WCHAR[ANYSIZE_ARRAY]`.
        let mut detail = vec![0u8; needed as usize];
        let header = detail.as_mut_ptr().cast::<u32>();
        unsafe { header.write(if cfg!(target_pointer_width = "64") { 8 } else { 6 }) };
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                detail.as_mut_ptr().cast(),
                needed,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            continue;
        }
        // The path is UTF-16 starting after the `cbSize` field.
        let path_bytes = &detail[4..];
        let path_u16: Vec<u16> = path_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|c| *c != 0)
            .collect();
        let path = String::from_utf16_lossy(&path_u16);
        if path.is_empty() {
            continue;
        }

        // Opening is the only way to ask what a HID device is on Windows. Done
        // for every HID device on the machine, which is why it is shared and
        // immediately closed — and why this is not called in a loop.
        let Ok(h) = open_path(&path) else { continue };
        let mut attrs = HiddAttributes {
            size: std::mem::size_of::<HiddAttributes>() as u32,
            ..HiddAttributes::default()
        };
        if unsafe { HidD_GetAttributes(h.0, &mut attrs) } == 0 {
            continue;
        }
        if !ids.is_empty() && !ids.contains(&(attrs.vendor_id, attrs.product_id)) {
            continue;
        }
        let mut name = [0u16; 128];
        let name = if unsafe { HidD_GetProductString(h.0, name.as_mut_ptr(), 256) } != 0 {
            from_wide(&name)
        } else {
            String::new()
        };
        let mut serial = [0u16; 128];
        let serial = if unsafe { HidD_GetSerialNumberString(h.0, serial.as_mut_ptr(), 256) } != 0 {
            from_wide(&serial)
        } else {
            String::new()
        };
        out.push(HidEntry {
            key: path,
            vendor: attrs.vendor_id,
            product: attrs.product_id,
            name,
            serial,
        });
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    out
}
