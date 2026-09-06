//! WASAPI capture: device enumeration and an event-driven 16 kHz mono f32 stream.
//!
//! Windows does the format conversion (`AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`), so whatever
//! the device's mix format is — 48 kHz stereo, or 16 kHz mono from a Bluetooth headset in
//! hands-free mode — the coordinator always receives what the engine wants.
//!
//! The stream is opened on demand and closed on release (see ARCHITECTURE §5): holding a
//! capture stream open keeps AirPods in the low-quality HFP profile.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use vox_core::config::DeviceSelection;
use vox_core::SAMPLE_RATE;
use windows::core::{w, IUnknown, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::misc::wide;
use crate::PlatformError;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    /// WASAPI endpoint ID — stable across reboots; what `DeviceSelection::Specific` stores.
    pub id: String,
    pub name: String,
    pub is_default_console: bool,
    pub is_default_communications: bool,
    /// A Bluetooth endpoint. Recording from one forces the headset into the hands-free
    /// profile, which audibly degrades its playback until the stream closes — see
    /// ARCHITECTURE §5. The UI warns about this.
    pub is_bluetooth: bool,
}

/// What the capture thread sends.
#[derive(Debug)]
pub enum AudioMsg {
    /// Mono f32 at [`SAMPLE_RATE`]; arbitrary length (whatever WASAPI delivered).
    Frames(Vec<f32>),
    /// The stream died (device unplugged, format change). No more frames will follow.
    Error(String),
}

/// Per-thread COM scope. `CoInitializeEx` is reference counted, so nesting is harmless.
pub(crate) struct ComScope {
    owns: bool,
}

impl ComScope {
    pub(crate) fn enter() -> ComScope {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComScope { owns: hr == S_OK }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owns {
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, PlatformError> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }
        .map_err(PlatformError::win("CoCreateInstance(MMDeviceEnumerator)"))
}

fn device_id(device: &IMMDevice) -> Result<String, PlatformError> {
    unsafe {
        let raw = device
            .GetId()
            .map_err(PlatformError::win("IMMDevice::GetId"))?;
        let id = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0 as _));
        Ok(id)
    }
}

/// Reads a string property from the endpoint's property store.
fn string_property(device: &IMMDevice, key: &windows::Win32::Foundation::PROPERTYKEY) -> String {
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return String::new();
        };
        let Ok(mut value) = store.GetValue(key) else {
            return String::new();
        };
        let inner = &value.Anonymous.Anonymous;
        let text = if inner.vt == VT_LPWSTR {
            inner.Anonymous.pwszVal.to_string().unwrap_or_default()
        } else {
            String::new()
        };
        let _ = PropVariantClear(&mut value);
        text
    }
}

fn friendly_name(device: &IMMDevice) -> String {
    string_property(device, &PKEY_Device_FriendlyName)
}

/// Bluetooth endpoints report `BTHENUM` (classic) or `BTHLEENUM` (LE) as their bus
/// enumerator; everything else (USB, HDAUDIO, …) is wired.
fn is_bluetooth(device: &IMMDevice) -> bool {
    string_property(device, &PKEY_Device_EnumeratorName)
        .to_ascii_uppercase()
        .starts_with("BTH")
}

fn default_id(
    enumerator: &IMMDeviceEnumerator,
    role: windows::Win32::Media::Audio::ERole,
) -> Option<String> {
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, role) }.ok()?;
    device_id(&device).ok()
}

/// Active capture endpoints, with which ones Windows currently treats as defaults.
pub fn list_capture_devices() -> Result<Vec<DeviceInfo>, PlatformError> {
    let _com = ComScope::enter();
    let enumerator = enumerator()?;
    let default_console = default_id(&enumerator, eConsole);
    let default_comms = default_id(&enumerator, eCommunications);

    let collection = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }
        .map_err(PlatformError::win("EnumAudioEndpoints"))?;
    let count = unsafe { collection.GetCount() }.map_err(PlatformError::win("GetCount"))?;

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let device = unsafe { collection.Item(i) }.map_err(PlatformError::win("Item"))?;
        let id = device_id(&device)?;
        out.push(DeviceInfo {
            is_default_console: default_console.as_deref() == Some(id.as_str()),
            is_default_communications: default_comms.as_deref() == Some(id.as_str()),
            name: friendly_name(&device),
            is_bluetooth: is_bluetooth(&device),
            id,
        });
    }
    Ok(out)
}

fn resolve(
    enumerator: &IMMDeviceEnumerator,
    selection: &DeviceSelection,
) -> Result<IMMDevice, PlatformError> {
    unsafe {
        match selection {
            DeviceSelection::DefaultCommunications => enumerator
                .GetDefaultAudioEndpoint(eCapture, eCommunications)
                .map_err(PlatformError::win(
                    "GetDefaultAudioEndpoint(communications)",
                )),
            DeviceSelection::DefaultConsole => enumerator
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .map_err(PlatformError::win("GetDefaultAudioEndpoint(console)")),
            DeviceSelection::Specific { id, .. } => {
                let w = wide(id);
                enumerator
                    .GetDevice(PCWSTR(w.as_ptr()))
                    .map_err(PlatformError::win("GetDevice"))
            }
        }
    }
}

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

/// A running capture. Dropping it stops the stream and joins the thread.
pub struct CaptureStream {
    stop: Arc<AtomicBool>,
    wake: SendHandle,
    join: Option<JoinHandle<()>>,
    pub device_name: String,
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        unsafe {
            let _ = SetEvent(self.wake.0);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        unsafe {
            let _ = CloseHandle(self.wake.0);
        }
    }
}

/// Opens the device and starts delivering [`AudioMsg`]s. Blocks until the stream is
/// running (or has failed), normally tens of milliseconds; Bluetooth profile switches can
/// take a few hundred.
pub fn open_capture(
    selection: &DeviceSelection,
    tx: Sender<AudioMsg>,
) -> Result<CaptureStream, PlatformError> {
    let stop = Arc::new(AtomicBool::new(false));
    let wake = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(PlatformError::win("CreateEventW"))?;
    let wake = SendHandle(wake);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<String, PlatformError>>();

    let selection = selection.clone();
    let stop_flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("vox-capture".into())
        .spawn(move || capture_thread(selection, tx, stop_flag, wake, ready_tx))
        .map_err(|e| PlatformError::Other(format!("spawning capture thread: {e}")))?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(device_name)) => Ok(CaptureStream {
            stop,
            wake,
            join: Some(join),
            device_name,
        }),
        Ok(Err(e)) => {
            let _ = join.join();
            unsafe {
                let _ = CloseHandle(wake.0);
            }
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            unsafe {
                let _ = SetEvent(wake.0);
            }
            Err(PlatformError::Timeout("opening the capture device".into()))
        }
    }
}

fn capture_thread(
    selection: DeviceSelection,
    tx: Sender<AudioMsg>,
    stop: Arc<AtomicBool>,
    wake: SendHandle,
    ready: mpsc::Sender<Result<String, PlatformError>>,
) {
    let _com = ComScope::enter();

    let setup = || -> Result<(IAudioClient, IAudioCaptureClient, String), PlatformError> {
        let enumerator = enumerator()?;
        let device = resolve(&enumerator, &selection)?;
        let name = friendly_name(&device);
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(PlatformError::win("IMMDevice::Activate(IAudioClient)"))?;

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 1,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    0,
                    0,
                    &format,
                    None,
                )
                .map_err(PlatformError::win("IAudioClient::Initialize"))?;
            client
                .SetEventHandle(wake.0)
                .map_err(PlatformError::win("IAudioClient::SetEventHandle"))?;
        }
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(
            PlatformError::win("IAudioClient::GetService(IAudioCaptureClient)"),
        )?;
        unsafe { client.Start() }.map_err(PlatformError::win("IAudioClient::Start"))?;
        Ok((client, capture, name))
    };

    let (client, capture, name) = match setup() {
        Ok(x) => x,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(name));

    // Best effort: MMCSS "Pro Audio" scheduling for the capture loop.
    let mut task_index = 0u32;
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };

    let result = pump(&capture, &tx, &stop, wake);
    unsafe {
        let _ = client.Stop();
    }
    if let Err(e) = result {
        let _ = tx.send(AudioMsg::Error(e));
    }
}

fn pump(
    capture: &IAudioCaptureClient,
    tx: &Sender<AudioMsg>,
    stop: &AtomicBool,
    wake: SendHandle,
) -> Result<(), String> {
    loop {
        let waited = unsafe { WaitForSingleObject(wake.0, 200) };
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if waited != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let packet = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| format!("GetNextPacketSize: {e}"))?;
            if packet == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|e| format!("GetBuffer: {e}"))?;
            let samples = if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                vec![0.0f32; frames as usize]
            } else {
                // SAFETY: we asked for mono 32-bit float, so `frames` f32s are valid here
                // until ReleaseBuffer.
                unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize) }.to_vec()
            };
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer: {e}"))?;
            if tx.send(AudioMsg::Frames(samples)).is_err() {
                // Receiver gone; nothing left to do.
                return Ok(());
            }
        }
    }
}
