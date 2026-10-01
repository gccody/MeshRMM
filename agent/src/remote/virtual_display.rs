//! A virtual monitor for a console that has no display attached.
//!
//! Without a monitor Windows has no desktop to capture. The Agent bundles
//! SudoVDA, an Indirect Display Driver, and installs it the first time a
//! session finds the console without a display. For the rest of that session
//! the service keeps one virtual monitor at the viewer's
//! [`HeadlessResolution`]. The driver's watchdog removes the monitor a few
//! seconds after the service stops pinging it, for example after a crash.
//!
//! The service runs in Session 0, so it can add the monitor but not read or
//! change the console's display configuration. The capture helper does that
//! in the console session: [`console_has_display`] reports a headless console
//! and [`show`] waits for the monitor and sets its mode.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::HeadlessResolution;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{
    CRYPT_E_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, PCWSTR};

use crate::win32::{OwnedHandle, wide};

const DRIVER_INF: &[u8] = include_bytes!("../../assets/sudovda/SudoVDA.inf");
const DRIVER_LIBRARY: &[u8] = include_bytes!("../../assets/sudovda/SudoVDA.dll");
const DRIVER_CATALOG: &[u8] = include_bytes!("../../assets/sudovda/sudovda.cat");
/// SudoMaker's self-signed code-signing certificate, which signs the catalog.
/// Its only extended key usage is code signing.
const DRIVER_CERTIFICATE: &[u8] = include_bytes!("../../assets/sudovda/sudovda.cer");
/// Store names under `HKLM`: `Root` lets the signature validate, and
/// `TrustedPublisher` installs the driver without asking anyone.
const CERTIFICATE_STORES: [&str; 2] = ["Root", "TrustedPublisher"];
const HARDWARE_ID: &str = r"root\sudomaker\sudovda";
/// The driver's device interface.
const INTERFACE: GUID = GUID::from_u128(0xe5bcc234_1e0c_418a_a0d4_ef8b7501414d);
/// The IOCTL layout this module speaks: SudoVDA protocol 0.2.
const PROTOCOL: (u8, u8) = (0, 2);
/// Records what the Agent installed, so uninstalling removes only that.
const INSTALL_RECORD: &str = "virtual-display.json";

const IOCTL_ADD: u32 = control_code(0x800);
const IOCTL_REMOVE: u32 = control_code(0x801);
const IOCTL_PING: u32 = control_code(0x888);
const IOCTL_PROTOCOL_VERSION: u32 = control_code(0x8ff);

/// The driver's watchdog removes every virtual monitor after 3 s without an
/// IOCTL.
const PING_INTERVAL: Duration = Duration::from_secs(1);
const REFRESH_RATE: u32 = 60;
/// Device setup can finish after the driver installation call returns.
const DEVICE_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const fn control_code(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    refresh_rate: u32,
    monitor: GUID,
    device_name: [u8; 14],
    serial_number: [u8; 14],
}

#[repr(C)]
#[derive(Default)]
struct AddOutput {
    adapter: LUID,
    target_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct ProtocolVersion {
    major: u8,
    minor: u8,
    incremental: u8,
    test_build: u8,
}

/// The size the last viewer chose, packed as `width << 32 | height`. The
/// first capture of a session starts before the viewer sends its choice, so
/// this spares a reconnecting technician a virtual display that is replaced
/// at once.
static LAST_RESOLUTION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn last_resolution() -> HeadlessResolution {
    let packed = LAST_RESOLUTION.load(Ordering::Relaxed);
    Some(HeadlessResolution::new(
        (packed >> 32) as u32,
        packed as u32,
    ))
    .filter(|resolution| resolution.valid())
    .unwrap_or_default()
}

pub(crate) fn remember_resolution(resolution: HeadlessResolution) {
    let packed = (u64::from(resolution.width) << 32) | u64::from(resolution.height);
    LAST_RESOLUTION.store(packed, Ordering::Relaxed);
}

/// Identifies the virtual monitor in the console's display configuration, and
/// the mode it should have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadlessTarget {
    pub(crate) adapter_low: u32,
    pub(crate) adapter_high: i32,
    pub(crate) target_id: u32,
    pub(crate) resolution: HeadlessResolution,
}

/// A virtual monitor that exists until this is dropped.
pub(crate) struct VirtualDisplay {
    device: OwnedHandle,
    monitor: GUID,
    target: HeadlessTarget,
    stop_pinging: Option<mpsc::Sender<()>>,
    pinger: Option<JoinHandle<()>>,
}

impl VirtualDisplay {
    /// Adds a monitor of `resolution`, installing the driver first if needed.
    pub(crate) fn add(resolution: HeadlessResolution) -> anyhow::Result<Self> {
        anyhow::ensure!(
            resolution.valid(),
            "{} is not a supported virtual display size",
            resolution.label()
        );
        let device = match open_device()? {
            Some(device) => device,
            None => {
                install_driver().context("could not install the virtual display driver")?;
                wait_for_device()?
            }
        };
        let version: ProtocolVersion = control(&device, IOCTL_PROTOCOL_VERSION, &())
            .context("the virtual display driver did not report its version")?;
        anyhow::ensure!(
            (version.major, version.minor) == PROTOCOL,
            "the installed virtual display driver speaks protocol {}.{}, not {}.{}",
            version.major,
            version.minor,
            PROTOCOL.0,
            PROTOCOL.1
        );
        let monitor = monitor_guid(resolution);
        let added: AddOutput = control(
            &device,
            IOCTL_ADD,
            &AddParams {
                width: resolution.width,
                height: resolution.height,
                refresh_rate: REFRESH_RATE,
                monitor,
                device_name: ascii_field("MeshRMM"),
                serial_number: ascii_field(&format!("{}x{}", resolution.width, resolution.height)),
            },
        )
        .context("the virtual display driver refused to add a monitor")?;
        let target = HeadlessTarget {
            adapter_low: added.adapter.LowPart,
            adapter_high: added.adapter.HighPart,
            target_id: added.target_id,
            resolution,
        };
        let (stop_pinging, stopped) = mpsc::channel();
        // The thread ends before `device` is closed; see `drop`.
        let ping_handle = device.0.0 as usize;
        let pinger = thread::Builder::new()
            .name("meshrmm-virtual-display".into())
            .spawn(move || ping_until_stopped(HANDLE(ping_handle as *mut _), &stopped));
        let mut display = Self {
            device,
            monitor,
            target,
            stop_pinging: Some(stop_pinging),
            pinger: None,
        };
        // Dropping `display` removes the monitor if the thread can't start.
        display.pinger = Some(pinger.context("could not start the virtual display watchdog")?);
        tracing::info!(
            width = resolution.width,
            height = resolution.height,
            target_id = target.target_id,
            "added a virtual display to the headless console"
        );
        Ok(display)
    }

    pub(crate) fn target(&self) -> HeadlessTarget {
        self.target
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        drop(self.stop_pinging.take());
        if let Some(pinger) = self.pinger.take() {
            let _ = pinger.join();
        }
        match control::<_, ()>(&self.device, IOCTL_REMOVE, &self.monitor) {
            Ok(()) => tracing::info!("removed the virtual display"),
            Err(error) => tracing::warn!(%error, "could not remove the virtual display"),
        }
    }
}

fn ping_until_stopped(device: HANDLE, stopped: &mpsc::Receiver<()>) {
    let mut failing = false;
    while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(PING_INTERVAL) {
        let result = unsafe { DeviceIoControl(device, IOCTL_PING, None, 0, None, 0, None, None) };
        match result {
            Err(error) if !failing => {
                tracing::warn!(%error, "could not ping the virtual display driver");
                failing = true;
            }
            Err(_) => {}
            Ok(()) => failing = false,
        }
    }
}

/// Each size is a different monitor to Windows, which applies a new monitor's
/// preferred mode rather than one remembered from an earlier session.
fn monitor_guid(resolution: HeadlessResolution) -> GUID {
    GUID::from_values(
        (resolution.width << 16) | resolution.height,
        0x8f1d,
        0x4c7e,
        *b"MeshRMMv",
    )
}

/// A NUL-padded ASCII field; the driver reads at most 13 characters.
fn ascii_field(value: &str) -> [u8; 14] {
    let mut field = [0; 14];
    for (slot, byte) in field.iter_mut().zip(value.bytes().take(13)) {
        *slot = byte;
    }
    field
}

fn control<I, O: Default>(device: &OwnedHandle, code: u32, input: &I) -> anyhow::Result<O> {
    let mut output = O::default();
    let mut returned = 0;
    unsafe {
        DeviceIoControl(
            device.0,
            code,
            (size_of::<I>() > 0).then_some((input as *const I).cast()),
            size_of::<I>() as u32,
            (size_of::<O>() > 0).then_some((&mut output as *mut O).cast()),
            size_of::<O>() as u32,
            Some(&mut returned),
            None,
        )
    }?;
    anyhow::ensure!(
        returned as usize == size_of::<O>(),
        "the virtual display driver returned {returned} bytes, not {}",
        size_of::<O>()
    );
    Ok(output)
}

fn open_device() -> anyhow::Result<Option<OwnedHandle>> {
    let mut length = 0;
    let result = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut length,
            &INTERFACE,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    anyhow::ensure!(
        result == CR_SUCCESS,
        "could not list virtual display devices ({})",
        result.0
    );
    let mut list = vec![0_u16; length as usize];
    let result = unsafe {
        CM_Get_Device_Interface_ListW(
            &INTERFACE,
            PCWSTR::null(),
            &mut list,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    anyhow::ensure!(
        result == CR_SUCCESS,
        "could not list virtual display devices ({})",
        result.0
    );
    let Some(path) = list.split(|c| *c == 0).find(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let path: Vec<u16> = path.iter().copied().chain(Some(0)).collect();
    let device = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .context("could not open the virtual display driver")?;
    Ok(Some(OwnedHandle(device)))
}

fn wait_for_device() -> anyhow::Result<OwnedHandle> {
    let deadline = Instant::now() + DEVICE_READY_TIMEOUT;
    loop {
        if let Some(device) = open_device()? {
            return Ok(device);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the virtual display driver was installed but its device did not start"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

/// What [`install_driver`] changed, so [`uninstall_driver`] removes only that.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct InstallRecord {
    /// The device node the Agent created.
    device: Option<String>,
    /// The driver package's published name, such as `oem42.inf`, when the
    /// Agent added it to the driver store.
    driver_package: Option<String>,
    /// Certificate stores the Agent added the signing certificate to.
    certificates: Vec<String>,
}

fn install_record_path() -> anyhow::Result<PathBuf> {
    Ok(crate::installer::config_directory()?.join(INSTALL_RECORD))
}

/// Installs the bundled driver on a new device node. Another application's
/// SudoVDA device would already provide the interface, so this runs only when
/// none is running.
fn install_driver() -> anyhow::Result<()> {
    let path = install_record_path()?;
    let previous = read_record(&path).unwrap_or_default();
    // A device the Agent created can be retried, for example after the
    // driver failed to install on it.
    let create_device = !device_node_exists()?;
    anyhow::ensure!(
        create_device || previous.device.is_some(),
        "a SudoVDA virtual display device exists but is not running; check it in Device Manager"
    );
    tracing::info!("installing the virtual display driver");
    let directory = crate::installer::config_directory()?.join("virtual-display");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    for (name, bytes) in [
        ("SudoVDA.inf", DRIVER_INF),
        ("SudoVDA.dll", DRIVER_LIBRARY),
        ("sudovda.cat", DRIVER_CATALOG),
    ] {
        crate::installer::replace_file(&directory.join(name), bytes)?;
    }

    let mut record = previous;
    let result = install_package(&directory.join("SudoVDA.inf"), create_device, &mut record);
    // Save what was changed even when a later step failed, so uninstalling
    // can still undo it.
    let saved = crate::installer::replace_file(&path, &serde_json::to_vec_pretty(&record)?);
    let _ = std::fs::remove_dir_all(&directory);
    result.and(saved)
}

fn install_package(
    inf: &Path,
    create_device: bool,
    record: &mut InstallRecord,
) -> anyhow::Result<()> {
    for store in CERTIFICATE_STORES {
        if add_certificate(store)? && !record.certificates.iter().any(|s| s == store) {
            record.certificates.push(store.to_owned());
        }
    }
    if let Some(package) = stage_package(inf)? {
        record.driver_package = Some(package);
    }
    if create_device {
        record.device = Some(create_device_node()?);
    }
    let inf = wide(inf);
    let hardware_id = wide(HARDWARE_ID);
    let mut reboot = windows::core::BOOL(0);
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hardware_id.as_ptr()),
            PCWSTR(inf.as_ptr()),
            UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
            Some(&mut reboot),
        )
    }
    .context("Windows did not install the virtual display driver")?;
    tracing::info!(
        reboot_required = reboot.as_bool(),
        "installed the virtual display driver"
    );
    Ok(())
}

fn read_record(path: &Path) -> Option<InstallRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Adds the signing certificate to a machine store. Returns false when it was
/// already there.
fn add_certificate(store: &str) -> anyhow::Result<bool> {
    let store_handle = open_certificate_store(store)?;
    let result = unsafe {
        CertAddEncodedCertificateToStore(
            Some(store_handle),
            X509_ASN_ENCODING,
            DRIVER_CERTIFICATE,
            CERT_STORE_ADD_NEW,
            None,
        )
    };
    let _ = unsafe { CertCloseStore(Some(store_handle), 0) };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.code() == CRYPT_E_EXISTS => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("could not trust the driver publisher in {store}"))
        }
    }
}

fn remove_certificate(store: &str) -> anyhow::Result<()> {
    let store_handle = open_certificate_store(store)?;
    let result = unsafe {
        let wanted = CertCreateCertificateContext(X509_ASN_ENCODING, DRIVER_CERTIFICATE);
        if wanted.is_null() {
            Err(windows::core::Error::from_thread())
        } else {
            let found = CertFindCertificateInStore(
                store_handle,
                X509_ASN_ENCODING,
                0,
                CERT_FIND_EXISTING,
                Some(wanted.cast()),
                None,
            );
            let _ = CertFreeCertificateContext(Some(wanted));
            // Deleting frees `found`.
            if found.is_null() {
                Ok(())
            } else {
                CertDeleteCertificateFromStore(found)
            }
        }
    };
    let _ = unsafe { CertCloseStore(Some(store_handle), 0) };
    result.with_context(|| format!("could not remove the driver publisher from {store}"))
}

fn open_certificate_store(store: &str) -> anyhow::Result<HCERTSTORE> {
    let name = wide(store);
    unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE),
            Some(name.as_ptr().cast()),
        )
    }
    .with_context(|| format!("could not open the {store} certificate store"))
}

/// Adds the package to the driver store. Returns its published name when it
/// wasn't there already.
fn stage_package(inf: &Path) -> anyhow::Result<Option<String>> {
    let inf = wide(inf);
    let mut published = [0_u16; 260];
    let mut component = windows::core::PWSTR::null();
    let result = unsafe {
        SetupCopyOEMInfW(
            PCWSTR(inf.as_ptr()),
            PCWSTR::null(),
            SPOST_PATH,
            SP_COPY_NOOVERWRITE,
            Some(&mut published),
            None,
            Some(&mut component),
        )
    };
    match result {
        Ok(()) => Ok(Some(unsafe { component.to_string() }?)),
        Err(error) if error.code() == ERROR_FILE_EXISTS.to_hresult() => Ok(None),
        Err(error) => Err(error).context("could not add the driver to the driver store"),
    }
}

/// Creates the root-enumerated device that the driver binds to. Returns its
/// instance ID.
fn create_device_node() -> anyhow::Result<String> {
    let devices =
        DeviceInfoList(unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), None) }?);
    let mut device = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    let class_name = wide("Display");
    unsafe {
        SetupDiCreateDeviceInfoW(
            devices.0,
            PCWSTR(class_name.as_ptr()),
            &GUID_DEVCLASS_DISPLAY,
            PCWSTR::null(),
            None,
            DICD_GENERATE_ID,
            Some(&mut device),
        )
    }
    .context("could not create the virtual display device")?;
    // REG_MULTI_SZ: the ID and an empty string.
    let hardware_ids: Vec<u8> = wide(HARDWARE_ID)
        .into_iter()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            devices.0,
            &mut device,
            SPDRP_HARDWAREID,
            Some(&hardware_ids),
        )
    }?;
    unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, devices.0, Some(&device)) }
        .context("could not register the virtual display device")?;
    instance_id(&devices, &device)
}

fn instance_id(devices: &DeviceInfoList, device: &SP_DEVINFO_DATA) -> anyhow::Result<String> {
    let mut id = [0_u16; 512];
    let mut length = 0;
    unsafe { SetupDiGetDeviceInstanceIdW(devices.0, device, Some(&mut id), Some(&mut length)) }?;
    let end = id.iter().position(|c| *c == 0).unwrap_or(id.len());
    Ok(String::from_utf16_lossy(&id[..end]))
}

/// Whether any display device, present or not, has SudoVDA's hardware ID.
fn device_node_exists() -> anyhow::Result<bool> {
    let devices = DeviceInfoList(unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )
    }?);
    let mut index = 0;
    loop {
        let mut device = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInfo(devices.0, index, &mut device) }.is_err() {
            return Ok(false);
        }
        index += 1;
        let mut buffer = [0_u8; 1024];
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                devices.0,
                &device,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buffer),
                None,
            )
        }
        .is_err()
        {
            continue;
        }
        let ids: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if ids
            .split(|c| *c == 0)
            .any(|id| String::from_utf16_lossy(id).eq_ignore_ascii_case(HARDWARE_ID))
        {
            return Ok(true);
        }
    }
}

struct DeviceInfoList(HDEVINFO);

impl Drop for DeviceInfoList {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Removes the device, driver package and certificates the Agent installed.
/// Leaves a SudoVDA installation that belongs to another application alone.
pub(crate) fn uninstall_driver() -> anyhow::Result<()> {
    let path = install_record_path()?;
    let Some(record) = read_record(&path) else {
        return Ok(());
    };
    let mut failures = Vec::new();
    if let Some(device) = &record.device
        && let Err(error) = remove_device(device)
    {
        failures.push(format!("{error:#}"));
    }
    if let Some(package) = &record.driver_package {
        let package = wide(package);
        if let Err(error) =
            unsafe { SetupUninstallOEMInfW(PCWSTR(package.as_ptr()), SUOI_FORCEDELETE, None) }.ok()
        {
            failures.push(format!("could not remove the driver package: {error}"));
        }
    }
    for store in &record.certificates {
        if let Err(error) = remove_certificate(store) {
            failures.push(format!("{error:#}"));
        }
    }
    let _ = std::fs::remove_file(&path);
    anyhow::ensure!(
        failures.is_empty(),
        "the virtual display driver was not fully removed: {}",
        failures.join("; ")
    );
    Ok(())
}

fn remove_device(instance_id: &str) -> anyhow::Result<()> {
    let devices =
        DeviceInfoList(unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), None) }?);
    let mut device = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    let id = wide(instance_id);
    if unsafe { SetupDiOpenDeviceInfoW(devices.0, PCWSTR(id.as_ptr()), None, 0, Some(&mut device)) }
        .is_err()
    {
        // Already removed, for example in Device Manager.
        return Ok(());
    }
    unsafe { DiUninstallDevice(Default::default(), devices.0, &device, 0, None) }
        .context("could not remove the virtual display device")
}

/// `DISPLAYCONFIG_TARGET_FORCED_AVAILABILITY_BOOT`, `_PATH` and `_SYSTEM`:
/// Windows keeps the target active although nothing is connected to it.
const FORCED_AVAILABILITY: u32 = 0x4 | 0x8 | 0x10;

/// Whether the console shows its desktop on a monitor, from the console
/// session. When the last monitor is unplugged, Windows keeps its output
/// active and the desktop keeps its size, but marks the target as forced
/// available; such a path doesn't count. An unreadable configuration counts
/// as having a monitor, so capture proceeds as before.
pub(crate) fn console_has_display() -> bool {
    match active_paths() {
        Ok(paths) => paths
            .iter()
            .any(|path| connected(path.targetInfo.statusFlags)),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not check the console's monitors"
            );
            true
        }
    }
}

fn connected(target_status: u32) -> bool {
    target_status & FORCED_AVAILABILITY == 0
}

fn active_paths() -> anyhow::Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
    let (mut path_count, mut mode_count) = (0, 0);
    unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) }
        .ok()
        .context("could not read the console's display configuration")?;
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    }
    .ok()
    .context("could not read the console's display configuration")?;
    paths.truncate(path_count as usize);
    Ok(paths)
}

/// Waits up to `timeout` for the virtual monitor to become active in the
/// console session, then gives it the requested mode. Windows may remember a
/// different mode for the monitor, for example one chosen in its display
/// settings during an earlier session.
pub(crate) fn show(target: &HeadlessTarget, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let source = loop {
        if let Some(source) = active_source(target)? {
            break source;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the virtual display did not become active within {} ms",
            timeout.as_millis()
        );
        thread::sleep(Duration::from_millis(50));
    };
    let mut current = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(PCWSTR(source.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current) }
        .ok()
        .context("could not read the virtual display's mode")?;
    let HeadlessResolution { width, height } = target.resolution;
    if (current.dmPelsWidth, current.dmPelsHeight) == (width, height) {
        return Ok(());
    }
    let wanted = DEVMODEW {
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT,
        dmPelsWidth: width,
        dmPelsHeight: height,
        ..current
    };
    // Not saved, so the console's own settings are unchanged.
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(source.as_ptr()),
            Some(&wanted),
            None,
            CDS_TYPE(0),
            None,
        )
    };
    anyhow::ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "{} was refused for the virtual display ({})",
        target.resolution.label(),
        result.0
    );
    tracing::info!(width, height, "set the virtual display's mode");
    Ok(())
}

/// The GDI device name (`\\.\DISPLAYn`) of the active path that shows
/// `target`, as a NUL-terminated string.
fn active_source(target: &HeadlessTarget) -> anyhow::Result<Option<Vec<u16>>> {
    let paths = active_paths()?;
    let Some(path) = paths.iter().find(|path| {
        path.targetInfo.adapterId.LowPart == target.adapter_low
            && path.targetInfo.adapterId.HighPart == target.adapter_high
            && path.targetInfo.id == target.target_id
    }) else {
        return Ok(None);
    };
    let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) };
    anyhow::ensure!(
        result == 0,
        "could not read the virtual display's device name ({result})"
    );
    let end = source
        .viewGdiDeviceName
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(source.viewGdiDeviceName.len());
    Ok(Some(
        source.viewGdiDeviceName[..end]
            .iter()
            .copied()
            .chain(Some(0))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_structures_match_the_sudovda_header() {
        assert_eq!(size_of::<AddParams>(), 56);
        assert_eq!(size_of::<AddOutput>(), 12);
        assert_eq!(size_of::<ProtocolVersion>(), 4);
        assert_eq!(IOCTL_ADD, 0x0022_2000);
        assert_eq!(IOCTL_REMOVE, 0x0022_2004);
        assert_eq!(IOCTL_PING, 0x0022_2220);
        assert_eq!(IOCTL_PROTOCOL_VERSION, 0x0022_23fc);
    }

    #[test]
    fn each_resolution_is_a_distinct_monitor() {
        let presets = HeadlessResolution::PRESETS.map(monitor_guid);
        for (index, guid) in presets.iter().enumerate() {
            assert!(!presets[index + 1..].contains(guid));
        }
        assert_eq!(
            monitor_guid(HeadlessResolution::HD),
            monitor_guid(HeadlessResolution::new(1280, 720))
        );
    }

    /// Run as an administrator after a test that installed the driver.
    #[test]
    #[ignore = "removes the virtual display driver the Agent installed"]
    fn uninstalling_removes_what_the_agent_installed() {
        let record = read_record(&install_record_path().unwrap()).expect("nothing was installed");
        assert!(record.device.is_some());
        uninstall_driver().unwrap();
        assert!(!device_node_exists().unwrap());
        assert!(open_device().unwrap().is_none());
        assert!(read_record(&install_record_path().unwrap()).is_none());
        for store in record.certificates {
            assert!(
                add_certificate(&store).unwrap(),
                "{store} still trusts SudoMaker"
            );
            remove_certificate(&store).unwrap();
        }
    }

    #[test]
    fn only_targets_windows_keeps_alive_count_as_disconnected() {
        // IN_USE alone is a connected monitor.
        assert!(connected(0x1));
        // An unplugged last monitor: IN_USE | FORCED_AVAILABILITY_SYSTEM.
        assert!(!connected(0x11));
        assert!(!connected(0x1 | 0x4));
        assert!(!connected(0x1 | 0x8));
        // FORCIBLE says what may be forced, not that it was.
        assert!(connected(0x1 | 0x2));
    }

    #[test]
    fn sessions_start_with_the_last_chosen_size() {
        assert_eq!(last_resolution(), HeadlessResolution::default());
        remember_resolution(HeadlessResolution::new(3840, 2160));
        assert_eq!(last_resolution(), HeadlessResolution::new(3840, 2160));
        remember_resolution(HeadlessResolution::default());
    }

    #[test]
    fn driver_strings_are_truncated_and_nul_terminated() {
        assert_eq!(&ascii_field("MeshRMM")[..8], b"MeshRMM\0");
        let long = ascii_field("abcdefghijklmnopq");
        assert_eq!(&long[..13], b"abcdefghijklm");
        assert_eq!(long[13], 0);
    }
}
