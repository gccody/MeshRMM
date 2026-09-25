//! A private, bounded, real-time ETW session for disk and network byte counts.
//! Never attaches to, changes, or stops another application's trace session.
use crate::win32::{OwnedHandle, wide};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::core::{GUID, PCWSTR, PWSTR};

const DISK: GUID = GUID::from_u128(0x3d6fa8d4_fe05_11d0_9dda_00c04fd7ba7c);
const TCP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);
#[derive(Default)]
pub struct Counters {
    pub bytes: HashMap<u32, (u64, u64)>,
    threads: HashMap<u32, u32>,
    pub lost: bool,
}
pub type Shared = Arc<Mutex<Counters>>;
#[repr(C)]
struct Properties {
    header: EVENT_TRACE_PROPERTIES,
    name: [u16; 128],
}
impl Properties {
    fn new() -> Self {
        let mut p = Self {
            header: EVENT_TRACE_PROPERTIES::default(),
            name: [0; 128],
        };
        p.header.Wnode.BufferSize = std::mem::size_of::<Self>() as u32;
        p.header.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        p.header.Wnode.ClientContext = 1;
        p.header.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        p
    }
}
fn name(pid: u32) -> Vec<u16> {
    wide(format!("MeshRMM-TaskManager-{pid}"))
}
pub fn stop(pid: u32) {
    let name = name(pid);
    let mut properties = Properties::new();
    unsafe {
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            &mut properties.header,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}
pub struct Monitor {
    pid: u32,
    pub counters: Shared,
}
impl Monitor {
    pub fn start() -> anyhow::Result<Self> {
        let pid = std::process::id();
        let mut name = name(pid);
        let mut properties = Properties::new();
        properties.header.Wnode.Guid = GUID::from_u128(uuid::Uuid::new_v4().as_u128());
        properties.header.BufferSize = 64;
        properties.header.MinimumBuffers = 4;
        properties.header.MaximumBuffers = 16;
        properties.header.FlushTimer = 1;
        properties.header.LogFileMode = EVENT_TRACE_REAL_TIME_MODE
            | EVENT_TRACE_SYSTEM_LOGGER_MODE
            | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING;
        properties.header.EnableFlags = EVENT_TRACE_FLAG_DISK_IO | EVENT_TRACE_FLAG_NETWORK_TCPIP;
        let mut session = CONTROLTRACE_HANDLE::default();
        unsafe {
            StartTraceW(&mut session, PCWSTR(name.as_ptr()), &mut properties.header).ok()?;
        }
        let counters = Arc::new(Mutex::new(Counters::default()));
        let monitor = Self {
            pid,
            counters: counters.clone(),
        };
        let context = Arc::into_raw(counters) as *mut core::ffi::c_void;
        let mut log = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_mut_ptr()),
            Context: context,
            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(event),
            },
            BufferCallback: Some(buffer),
            ..Default::default()
        };
        let trace = unsafe { OpenTraceW(&mut log) };
        if trace.Value == u64::MAX {
            unsafe {
                drop(Arc::from_raw(context as *const Mutex<Counters>));
            }
            return Err(windows::core::Error::from_thread().into());
        }
        let context = context as usize;
        let result = std::thread::Builder::new()
            .name("task-manager-telemetry".into())
            .spawn(move || unsafe {
                let _owner = Arc::from_raw(context as *const Mutex<Counters>);
                let result = ProcessTrace(&[trace], None, None);
                if result.is_err()
                    && let Ok(mut counters) = _owner.lock()
                {
                    counters.lost = true;
                }
                let _ = CloseTrace(trace);
            });
        if let Err(error) = result {
            unsafe {
                let _ = CloseTrace(trace);
                drop(Arc::from_raw(context as *const Mutex<Counters>));
            }
            return Err(error.into());
        }
        Ok(monitor)
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        stop(self.pid);
    }
}
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn decode(
    provider: GUID,
    opcode: u8,
    version: u8,
    pointer32: bool,
    bytes: &[u8],
    threads: &HashMap<u32, u32>,
) -> Option<(u32, u64, bool)> {
    if provider == DISK && matches!(opcode, 10 | 11) && version >= 3 {
        let size = u32_at(bytes, 8)?;
        let thread = u32_at(bytes, if pointer32 { 40 } else { 48 })?;
        let pid = *threads.get(&thread)?;
        Some((pid, size as u64, true))
    } else if (provider == TCP || provider == UDP)
        && matches!(opcode, 10 | 11 | 26 | 27)
        && version >= 1
    {
        Some((u32_at(bytes, 0)?, u32_at(bytes, 4)? as u64, false))
    } else {
        None
    }
}
unsafe extern "system" fn event(record: *mut EVENT_RECORD) {
    unsafe {
        if record.is_null() {
            return;
        }
        let record = &*record;
        if record.UserContext.is_null() || record.UserData.is_null() {
            return;
        }
        let shared = &*(record.UserContext as *const Mutex<Counters>);
        if let Ok(mut counters) = shared.lock() {
            let bytes = std::slice::from_raw_parts(
                record.UserData.cast::<u8>(),
                record.UserDataLength as usize,
            );
            if let Some((pid, size, disk)) = decode(
                record.EventHeader.ProviderId,
                record.EventHeader.EventDescriptor.Opcode,
                record.EventHeader.EventDescriptor.Version,
                record.EventHeader.Flags as u32 & EVENT_HEADER_FLAG_32_BIT_HEADER != 0,
                bytes,
                &counters.threads,
            ) {
                if counters.bytes.len() >= 65536 && !counters.bytes.contains_key(&pid) {
                    counters.lost = true;
                    return;
                }
                let entry = counters.bytes.entry(pid).or_default();
                if disk {
                    entry.0 = entry.0.saturating_add(size);
                } else {
                    entry.1 = entry.1.saturating_add(size);
                }
            }
        }
    }
}
unsafe extern "system" fn buffer(log: *mut EVENT_TRACE_LOGFILEW) -> u32 {
    unsafe {
        if !log.is_null()
            && (*log).EventsLost > 0
            && !(*log).Context.is_null()
            && let Ok(mut counters) = (&*((*log).Context as *const Mutex<Counters>)).lock()
        {
            counters.lost = true;
        }
    }
    1
}
pub fn update_threads(shared: &Shared) {
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return;
    };
    let snapshot = OwnedHandle(snapshot);
    let mut threads = HashMap::new();
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    unsafe {
        if Thread32First(snapshot.0, &mut entry).is_ok() {
            loop {
                threads.insert(entry.th32ThreadID, entry.th32OwnerProcessID);
                if Thread32Next(snapshot.0, &mut entry).is_err() {
                    break;
                }
            }
        }
    }
    if let Ok(mut counters) = shared.lock() {
        counters.threads = threads;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Requires administrator privileges; starts a private kernel telemetry session"]
    fn live_disk_network_and_trace_cleanup() {
        use std::io::{Read, Write};
        use std::os::windows::fs::OpenOptionsExt;
        let monitor = Monitor::start().unwrap();
        update_threads(&monitor.counters);
        let file =
            std::env::temp_dir().join(format!("meshrmm-telemetry-{}.tmp", uuid::Uuid::new_v4()));
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(0x80000000)
            .open(&file)
            .unwrap();
        output.write_all(&vec![0x55; 16 * 1024 * 1024]).unwrap();
        output.sync_all().unwrap();
        drop(output);
        std::fs::remove_file(file).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            stream.read_to_end(&mut data).unwrap();
            assert_eq!(data.len(), 1024 * 1024);
        });
        let mut client = std::net::TcpStream::connect(address).unwrap();
        client.write_all(&vec![0x44; 1024 * 1024]).unwrap();
        drop(client);
        server.join().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let result = loop {
            let counters = monitor.counters.lock().unwrap();
            let disk: u64 = counters.bytes.values().map(|v| v.0).sum();
            let network = counters.bytes.get(&std::process::id()).map_or(0, |v| v.1);
            if disk > 0 && network >= 1024 * 1024 && !counters.lost {
                break true;
            }
            if std::time::Instant::now() > deadline {
                eprintln!("disk={disk} network={network} lost={}", counters.lost);
                break false;
            }
            drop(counters);
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        drop(monitor);
        let name = name(std::process::id());
        let mut properties = Properties::new();
        assert!(
            unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE::default(),
                    PCWSTR(name.as_ptr()),
                    &mut properties.header,
                    EVENT_TRACE_CONTROL_QUERY,
                )
            }
            .is_err()
        );
        assert!(
            result,
            "Private ETW session did not deliver fixture disk/network traffic"
        );
    }
    #[test]
    fn decoder_checks_lengths_versions_and_issuing_thread() {
        let mut bytes = [0u8; 52];
        bytes[8..12].copy_from_slice(&4096u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&42u32.to_le_bytes());
        let threads = HashMap::from([(42, 123)]);
        assert_eq!(
            decode(DISK, 10, 3, false, &bytes, &threads),
            Some((123, 4096, true))
        );
        assert_eq!(decode(DISK, 10, 2, false, &bytes, &threads), None);
        assert_eq!(decode(DISK, 10, 3, false, &bytes[..51], &threads), None);
        assert_eq!(decode(DISK, 10, 3, false, &bytes, &HashMap::new()), None);
        bytes[..4].copy_from_slice(&123u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&50u32.to_le_bytes());
        assert_eq!(
            decode(TCP, 26, 2, false, &bytes, &threads),
            Some((123, 50, false))
        );
        assert_eq!(decode(TCP, 14, 2, false, &bytes, &threads), None);
    }
}
