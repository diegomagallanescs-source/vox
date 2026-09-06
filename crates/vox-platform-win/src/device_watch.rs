//! Audio device change notifications (`IMMNotificationClient`).
//!
//! Fires when a capture device appears, disappears, changes state, or when Windows picks a
//! new default — e.g. AirPods connecting. The settings UI uses it to refresh its device list
//! live; the coordinator resolves the device on every capture, so it needs nothing.

use std::sync::mpsc;
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl, DEVICE_STATE,
};
// The `#[implement]` macro expands to `windows_core::` paths, so it must be imported from the
// `windows-core` crate itself rather than through `windows::core`.
use windows_core::{implement, PCWSTR};

use crate::wasapi::{enumerator, ComScope};
use crate::PlatformError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A device was added, removed, enabled or disabled.
    ListChanged,
    /// Windows changed a default capture device (console or communications role).
    DefaultChanged,
}

#[implement(IMMNotificationClient)]
struct Client {
    tx: Sender<DeviceEvent>,
}

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows_core::Result<()> {
        let _ = self.tx.try_send(DeviceEvent::ListChanged);
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows_core::Result<()> {
        let _ = self.tx.try_send(DeviceEvent::ListChanged);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows_core::Result<()> {
        let _ = self.tx.try_send(DeviceEvent::ListChanged);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows_core::Result<()> {
        if flow == eCapture {
            let _ = self.tx.try_send(DeviceEvent::DefaultChanged);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows_core::Result<()> {
        Ok(())
    }
}

/// Keeps the notification registration alive. Dropping it unregisters.
pub struct DeviceWatcher {
    stop: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Starts listening. Events arrive on `tx` from COM threads; treat them as a hint to
/// re-enumerate, not as data.
pub fn watch_devices(tx: Sender<DeviceEvent>) -> Result<DeviceWatcher, PlatformError> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PlatformError>>();

    let join = std::thread::Builder::new()
        .name("vox-device-watch".into())
        .spawn(move || {
            let _com = ComScope::enter();
            let enumerator = match enumerator() {
                Ok(e) => e,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let client: IMMNotificationClient = Client { tx }.into();
            if let Err(e) = unsafe { enumerator.RegisterEndpointNotificationCallback(&client) } {
                let _ = ready_tx.send(Err(PlatformError::win(
                    "RegisterEndpointNotificationCallback",
                )(e)));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // Block until the watcher is dropped (sender closed) or told to stop.
            let _ = stop_rx.recv();
            unsafe {
                let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
            }
        })
        .map_err(|e| PlatformError::Other(format!("spawning device watcher: {e}")))?;

    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| PlatformError::Timeout("device watcher did not start".into()))??;

    Ok(DeviceWatcher {
        stop: Some(stop_tx),
        join: Some(join),
    })
}
