//! PlayStation 5 DualSense pads (and the DualSense Edge) read straight from
//! HID, so that they work without Steam Input or DS4Windows making them
//! XInput pads. USB and Bluetooth, rumble included. They take the host pad
//! slots XInput leaves empty, in the order they were connected.
//! `VICTORY_DUALSENSE=0` leaves them alone: for a host that already makes
//! them XInput pads, where each would otherwise be seen twice.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

pub(super) const VENDOR: u16 = 0x054C;
/// DualSense, DualSense Edge.
pub(super) const PRODUCTS: [u16; 2] = [0x0CE6, 0x0DF2];
/// The input report lengths Windows reads them with (report ID included).
pub(super) const USB_INPUT: usize = 64;
pub(super) const BLUETOOTH_INPUT: usize = 78;
/// The shortest output reports that hold the motor speeds.
const USB_OUTPUT: usize = 48;
const BLUETOOTH_OUTPUT: usize = 78;

/// Off with `VICTORY_DUALSENSE=0` (or `false`, `off`, `no`).
pub(super) fn enabled() -> bool {
    std::env::var("VICTORY_DUALSENSE").map_or(true, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// An input report as a console pad; `None` for reports of other kinds.
/// Raw values, no dead zone: titles apply their own.
pub(super) fn parse(report: &[u8], bluetooth: bool) -> Option<PadState> {
    // Where the sticks, the triggers and the three button bytes are.
    let (sticks, triggers, buttons) = match (*report.first()?, bluetooth) {
        (0x01, false) => (1, 5, 8),
        (0x31, true) => (2, 6, 9),
        // Bluetooth before the full report was asked for: the short form.
        (0x01, true) => (1, 8, 5),
        _ => return None,
    };
    let at = |i: usize| report.get(i).copied();
    let stick = |v: u8| ((f32::from(v) - 128.0) / 127.0).clamp(-1.0, 1.0);
    let (face, shoulders, system) = (at(buttons)?, at(buttons + 1)?, at(buttons + 2)?);
    let dpad = match face & 0x0F {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => DOWN | RIGHT,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => UP | LEFT,
        _ => 0,
    };
    // Square, cross, circle, triangle as X, A, B, Y (where they sit);
    // Create and the touchpad's click as View, Options as Menu.
    let table = [
        (face, 0x10, X),
        (face, 0x20, A),
        (face, 0x40, B),
        (face, 0x80, Y),
        (shoulders, 0x01, LEFT_SHOULDER),
        (shoulders, 0x02, RIGHT_SHOULDER),
        (shoulders, 0x10, VIEW),
        (shoulders, 0x20, MENU),
        (shoulders, 0x40, LEFT_STICK),
        (shoulders, 0x80, RIGHT_STICK),
        (system, 0x02, VIEW),
    ];
    Some(PadState {
        buttons: table
            .iter()
            .filter(|(byte, mask, _)| byte & mask != 0)
            .fold(dpad, |all, (_, _, bit)| all | bit),
        left_trigger: f32::from(at(triggers)?) / 255.0,
        right_trigger: f32::from(at(triggers + 1)?) / 255.0,
        left_x: stick(at(sticks)?),
        // HID counts down from the top; the console counts up.
        left_y: -stick(at(sticks + 1)?),
        right_x: stick(at(sticks + 2)?),
        right_y: -stick(at(sticks + 3)?),
    })
}

/// The output report that sets the motors (`low`: the big slow one on the
/// left, `high`: the small fast one on the right), `len` bytes long as the
/// device reports. `seq` is Bluetooth's 4-bit sequence number.
pub(super) fn rumble_report(
    len: usize,
    bluetooth: bool,
    seq: u8,
    low: u8,
    high: u8,
) -> Option<Vec<u8>> {
    if len
        < if bluetooth {
            BLUETOOTH_OUTPUT
        } else {
            USB_OUTPUT
        }
    {
        return None;
    }
    let mut out = vec![0_u8; len];
    let common = if bluetooth {
        out[0] = 0x31;
        out[1] = (seq & 0x0F) << 4;
        out[2] = 0x10;
        3
    } else {
        out[0] = 0x02;
        1
    };
    // Rumble the way older pads do, with the haptics behind it.
    out[common] = 0x01 | 0x02;
    out[common + 2] = high;
    out[common + 3] = low;
    if bluetooth {
        // Over the first 74 bytes, after the transaction header 0xA2.
        let crc = crc32(&[&[0xA2], &out[..BLUETOOTH_OUTPUT - 4]]);
        out[BLUETOOTH_OUTPUT - 4..BLUETOOTH_OUTPUT].copy_from_slice(&crc.to_le_bytes());
    }
    Some(out)
}

/// CRC-32 (the zlib one) of the parts one after another.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0_u32;
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(windows)]
pub(super) use host::Pads;

#[cfg(windows)]
mod host {
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData,
        HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps,
        PHIDP_PREPARSED_DATA,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
    };
    use windows_sys::core::GUID;

    use super::super::lock;
    use super::{
        BLUETOOTH_INPUT, PRODUCTS, PadState, USB_INPUT, VENDOR, enabled, parse, rumble_report,
    };

    /// How often pads plugged in or paired later are looked for.
    const SCAN: Duration = Duration::from_secs(2);

    /// What a pad's reader thread shares.
    struct Shared {
        /// The latest report; `None` until the first.
        state: Mutex<Option<PadState>>,
        /// Cleared when a read fails: unplugged or out of range.
        alive: AtomicBool,
    }

    struct Entry {
        path: Vec<u16>,
        shared: Arc<Shared>,
        /// A handle of its own: a read waiting on the reader's would hold
        /// writes up (the handles are synchronous).
        writer: usize,
        bluetooth: bool,
        output: usize,
        seq: u8,
        rumble: (u8, u8),
    }

    impl Entry {
        fn connected(&self) -> Option<PadState> {
            if self.shared.alive.load(Ordering::Acquire) {
                *lock(&self.shared.state)
            } else {
                None
            }
        }
    }

    impl Drop for Entry {
        fn drop(&mut self) {
            // SAFETY: ours, closed once.
            unsafe { CloseHandle(self.writer as HANDLE) };
        }
    }

    /// The DualSense pads connected, looked for from a thread of their own
    /// (the device list is slow to ask for) once the first pad is asked for.
    #[derive(Default)]
    pub(in super::super) struct Pads {
        entries: Arc<Mutex<Vec<Entry>>>,
        started: bool,
    }

    impl Pads {
        fn start(&mut self) {
            if std::mem::replace(&mut self.started, true) {
                return;
            }
            if !enabled() {
                tracing::info!("DualSense pads left alone (VICTORY_DUALSENSE=0)");
                return;
            }
            let entries = Arc::clone(&self.entries);
            let spawned = std::thread::Builder::new()
                .name("victory dualsense".into())
                .spawn(move || {
                    loop {
                        scan(&entries);
                        std::thread::sleep(SCAN);
                    }
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "DualSense pads not looked for");
            }
        }

        /// The `index`th DualSense connected, `None` past the last.
        pub(in super::super) fn pad(&mut self, index: usize) -> Option<PadState> {
            self.start();
            lock(&self.entries)
                .iter()
                .filter_map(Entry::connected)
                .nth(index)
        }

        pub(in super::super) fn rumble(&mut self, index: usize, low: u16, high: u16) {
            let mut entries = lock(&self.entries);
            let Some(entry) = entries
                .iter_mut()
                .filter(|e| e.connected().is_some())
                .nth(index)
            else {
                return;
            };
            let speeds = ((low >> 8) as u8, (high >> 8) as u8);
            if entry.rumble == speeds {
                return;
            }
            entry.seq = (entry.seq + 1) & 0x0F;
            let Some(report) =
                rumble_report(entry.output, entry.bluetooth, entry.seq, speeds.0, speeds.1)
            else {
                return;
            };
            let mut written = 0;
            // SAFETY: the report outlives the synchronous write.
            if unsafe {
                WriteFile(
                    entry.writer as HANDLE,
                    report.as_ptr(),
                    report.len() as u32,
                    &mut written,
                    null_mut(),
                )
            } != 0
            {
                entry.rumble = speeds;
            }
        }
    }

    fn scan(entries: &Mutex<Vec<Entry>>) {
        lock(entries).retain(|entry| entry.shared.alive.load(Ordering::Acquire));
        for path in hid_paths() {
            if lock(entries).iter().any(|entry| entry.path == path) {
                continue;
            }
            if let Some(entry) = open(path) {
                lock(entries).push(entry);
            }
        }
    }

    /// Every HID device interface present, as nul-terminated paths.
    fn hid_paths() -> Vec<Vec<u16>> {
        let mut guid = GUID::from_u128(0);
        // SAFETY: room for the GUID.
        unsafe { HidD_GetHidGuid(&mut guid) };
        // SAFETY: plain query.
        let set = unsafe {
            SetupDiGetClassDevsW(
                &guid,
                null(),
                null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if set == -1 {
            return Vec::new();
        }
        let mut paths = Vec::new();
        for index in 0.. {
            // SAFETY: plain data.
            let mut data: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
            data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            // SAFETY: a live set and room for the data.
            if unsafe { SetupDiEnumDeviceInterfaces(set, null(), &guid, index, &mut data) } == 0 {
                break;
            }
            let mut size = 0_u32;
            // SAFETY: asks for the size only.
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &data, null_mut(), 0, &mut size, null_mut())
            };
            let header = size_of::<u32>();
            if (size as usize) <= header {
                continue;
            }
            // u64s, for the detail's alignment.
            let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
            let detail = buffer
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            // SAFETY: the buffer holds `size` bytes, aligned.
            unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
            // SAFETY: as above.
            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &data, detail, size, null_mut(), null_mut())
            } == 0
            {
                continue;
            }
            let room = (size as usize - header) / 2;
            // SAFETY: the path follows the size field, inside the buffer.
            let chars = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(header).cast::<u16>(),
                    room,
                )
            };
            let len = chars.iter().position(|&c| c == 0).unwrap_or(room);
            let mut path = chars[..len].to_vec();
            path.push(0);
            paths.push(path);
        }
        // SAFETY: ours, destroyed once.
        unsafe { SetupDiDestroyDeviceInfoList(set) };
        paths
    }

    fn create(path: &[u16], access: u32) -> Option<HANDLE> {
        // SAFETY: a nul-terminated path.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        (handle != INVALID_HANDLE_VALUE).then_some(handle)
    }

    fn caps(handle: HANDLE) -> Option<HIDP_CAPS> {
        let mut data: PHIDP_PREPARSED_DATA = 0;
        // SAFETY: an open device; room for the pointer.
        if !unsafe { HidD_GetPreparsedData(handle, &mut data) } {
            return None;
        }
        // SAFETY: plain data.
        let mut caps: HIDP_CAPS = unsafe { zeroed() };
        // SAFETY: the data just asked for, freed once.
        let status = unsafe { HidP_GetCaps(data, &mut caps) };
        unsafe { HidD_FreePreparsedData(data) };
        (status == HIDP_STATUS_SUCCESS).then_some(caps)
    }

    /// The device at `path` if it is a DualSense, with its reader running.
    fn open(path: Vec<u16>) -> Option<Entry> {
        // Without access: enough to ask what it is, and refused by no one.
        let probe = create(&path, 0)?;
        // SAFETY: plain data.
        let mut attributes: HIDD_ATTRIBUTES = unsafe { zeroed() };
        attributes.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
        // SAFETY: an open device; room for the attributes.
        let ours = unsafe { HidD_GetAttributes(probe, &mut attributes) }
            && attributes.VendorID == VENDOR
            && PRODUCTS.contains(&attributes.ProductID);
        let caps = if ours { caps(probe) } else { None };
        // SAFETY: ours, closed once.
        unsafe { CloseHandle(probe) };
        let caps = caps?;
        let input = usize::from(caps.InputReportByteLength);
        let bluetooth = match input {
            USB_INPUT => false,
            BLUETOOTH_INPUT => true,
            _ => return None,
        };
        let reader = create(&path, GENERIC_READ | GENERIC_WRITE)?;
        let Some(writer) = create(&path, GENERIC_READ | GENERIC_WRITE) else {
            // SAFETY: ours, closed once.
            unsafe { CloseHandle(reader) };
            return None;
        };
        let feature = usize::from(caps.FeatureReportByteLength);
        if bluetooth && feature > 0 {
            // Asking for the calibration makes a pad paired over Bluetooth
            // send its full reports (0x31) instead of the short ones.
            let mut report = vec![0_u8; feature];
            report[0] = 0x05;
            // SAFETY: `feature` bytes of room.
            unsafe { HidD_GetFeature(reader, report.as_mut_ptr().cast(), feature as u32) };
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(None),
            alive: AtomicBool::new(true),
        });
        let theirs = Arc::clone(&shared);
        let handle = reader as usize;
        let spawned = std::thread::Builder::new()
            .name("victory dualsense read".into())
            .spawn(move || read(handle, input, bluetooth, &theirs));
        if spawned.is_err() {
            // SAFETY: ours, closed once.
            unsafe {
                CloseHandle(reader);
                CloseHandle(writer);
            }
            return None;
        }
        tracing::info!(
            product = format_args!("{:04x}", attributes.ProductID),
            bluetooth,
            "DualSense connected"
        );
        Some(Entry {
            path,
            shared,
            writer: writer as usize,
            bluetooth,
            output: usize::from(caps.OutputReportByteLength),
            seq: 0,
            rumble: (0, 0),
        })
    }

    /// Reads reports until the pad goes, then closes its handle.
    fn read(handle: usize, len: usize, bluetooth: bool, shared: &Shared) {
        let handle = handle as HANDLE;
        let mut report = vec![0_u8; len];
        loop {
            let mut got = 0_u32;
            // SAFETY: `len` bytes of room; a synchronous read.
            if unsafe {
                ReadFile(
                    handle,
                    report.as_mut_ptr(),
                    len as u32,
                    &mut got,
                    null_mut(),
                )
            } == 0
            {
                break;
            }
            if let Some(pad) = parse(&report[..(got as usize).min(len)], bluetooth) {
                *lock(&shared.state) = Some(pad);
            }
        }
        shared.alive.store(false, Ordering::Release);
        // SAFETY: ours, closed once.
        unsafe { CloseHandle(handle) };
        tracing::info!("DualSense disconnected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(sticks: [u8; 4], triggers: [u8; 2], buttons: [u8; 3]) -> Vec<u8> {
        let mut report = vec![0_u8; USB_INPUT];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&sticks);
        report[5..7].copy_from_slice(&triggers);
        report[8..11].copy_from_slice(&buttons);
        report
    }

    #[test]
    fn usb_reports_become_console_pads() {
        // Cross, Options, L1, touchpad click; D-pad down-left.
        let pad = parse(
            &usb([0, 255, 128, 128], [255, 0], [0x20 | 5, 0x20 | 0x01, 0x02]),
            false,
        )
        .unwrap();
        assert_eq!(pad.buttons, A | MENU | LEFT_SHOULDER | VIEW | DOWN | LEFT);
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (-1.0, -1.0, 0.0, 0.0)
        );
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
    }

    #[test]
    fn nothing_held_is_nothing_pressed() {
        let pad = parse(&usb([128; 4], [0, 0], [0x08, 0, 0]), false).unwrap();
        assert_eq!(pad, PadState::default());
    }

    #[test]
    fn bluetooth_full_and_short_reports() {
        let mut full = vec![0_u8; BLUETOOTH_INPUT];
        full[0] = 0x31;
        full[2..6].copy_from_slice(&[128, 0, 128, 128]);
        full[7] = 255;
        full[9] = 0x80 | 0x08; // triangle, no D-pad
        let pad = parse(&full, true).unwrap();
        assert_eq!((pad.buttons, pad.left_y, pad.right_trigger), (Y, 1.0, 1.0));

        let mut short = vec![0_u8; BLUETOOTH_INPUT];
        short[0] = 0x01;
        short[1..5].copy_from_slice(&[128; 4]);
        short[5] = 0x40 | 0x08; // circle
        short[6] = 0x80; // R3
        short[8] = 255;
        let pad = parse(&short, true).unwrap();
        assert_eq!((pad.buttons, pad.left_trigger), (B | RIGHT_STICK, 1.0));
    }

    #[test]
    fn other_reports_are_skipped() {
        assert_eq!(parse(&[0x05; 64], false), None);
        assert_eq!(parse(&[], true), None);
    }

    /// With a DualSense connected: `cargo test -p victory-hle real_dualsense -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn real_dualsense() {
        let mut pads = Pads::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut seen = None;
        while std::time::Instant::now() < deadline {
            if let Some(pad) = pads.pad(0) {
                if seen != Some(pad) {
                    println!("{pad:?}");
                    seen = Some(pad);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        pads.rumble(0, 0x8000, 0x8000);
        std::thread::sleep(std::time::Duration::from_millis(400));
        pads.rumble(0, 0, 0);
        assert!(seen.is_some(), "no DualSense read");
    }

    #[test]
    fn rumble_reports() {
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
        let usb = rumble_report(48, false, 0, 200, 100).unwrap();
        assert_eq!(&usb[..5], &[0x02, 0x03, 0x00, 100, 200]);
        let bt = rumble_report(78, true, 3, 200, 100).unwrap();
        assert_eq!(&bt[..7], &[0x31, 0x30, 0x10, 0x03, 0x00, 100, 200]);
        let crc = crc32(&[&[0xA2], &bt[..74]]);
        assert_eq!(bt[74..], crc.to_le_bytes());
        assert_eq!(rumble_report(10, true, 0, 0, 0), None);
    }
}
