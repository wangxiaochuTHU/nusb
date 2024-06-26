use std::{
    ffi::c_void,
    io::{self, ErrorKind},
    mem::size_of_val,
    os::windows::prelude::OwnedHandle,
    ptr::null_mut,
    sync::Arc,
    time::Duration,
};

use log::{debug, error, info, warn};
use windows_sys::Win32::{
    Devices::Usb::{
        WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetAssociatedInterface, WinUsb_Initialize,
        WinUsb_ResetPipe, WinUsb_SetCurrentAlternateSetting, WinUsb_SetPipePolicy,
        IGNORE_SHORT_PACKETS, PIPE_TRANSFER_TIMEOUT, RAW_IO, WINUSB_INTERFACE_HANDLE,
        WINUSB_SETUP_PACKET,
    },
    Foundation::{GetLastError, FALSE, TRUE},
};

use crate::{
    descriptors::{validate_config_descriptor, DESCRIPTOR_TYPE_CONFIGURATION},
    transfer::{Control, Direction, EndpointType, Recipient, TransferError, TransferHandle},
    DeviceInfo, Error,
};

use super::{
    enumeration::find_device_interface_path,
    hub::HubPort,
    util::{create_file, raw_handle},
    DevInst,
};

pub(crate) struct WindowsDevice {
    config_descriptors: Vec<Vec<u8>>,
    active_config: u8,
    devinst: DevInst,
}

impl WindowsDevice {
    pub(crate) fn from_device_info(d: &DeviceInfo) -> Result<Arc<WindowsDevice>, Error> {
        debug!("Creating device for {:?}", d.instance_id);

        // Look up the device again in case the DeviceInfo is stale. In
        // particular, don't trust its `port_number` because another device
        // might now be connected to that port, and we'd get its descriptors
        // instead.
        let hub_port = HubPort::by_child_devinst(d.devinst)?;
        let connection_info = hub_port.get_node_connection_info()?;
        let num_configurations = connection_info.DeviceDescriptor.bNumConfigurations;

        let config_descriptors = (0..num_configurations)
            .flat_map(|i| {
                let res = hub_port.get_descriptor(DESCRIPTOR_TYPE_CONFIGURATION, i, 0);
                match res {
                    Ok(v) => validate_config_descriptor(&v[..]).map(|_| v),
                    Err(e) => {
                        error!("Failed to read config descriptor {}: {}", i, e);
                        None
                    }
                }
            })
            .collect();

        Ok(Arc::new(WindowsDevice {
            config_descriptors,
            active_config: connection_info.CurrentConfigurationValue,
            devinst: d.devinst,
        }))
    }

    pub(crate) fn active_configuration_value(&self) -> u8 {
        self.active_config
    }

    pub(crate) fn configuration_descriptors(&self) -> impl Iterator<Item = &[u8]> {
        self.config_descriptors.iter().map(|d| &d[..])
    }

    pub(crate) fn set_configuration(&self, _configuration: u8) -> Result<(), Error> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "set_configuration not supported by WinUSB",
        ))
    }

    pub(crate) fn get_descriptor(
        &self,
        desc_type: u8,
        desc_index: u8,
        language_id: u16,
    ) -> Result<Vec<u8>, Error> {
        HubPort::by_child_devinst(self.devinst)?.get_descriptor(desc_type, desc_index, language_id)
    }

    pub(crate) fn reset(&self) -> Result<(), Error> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "reset not supported by WinUSB",
        ))
    }

    pub(crate) fn claim_interface(
        self: &Arc<Self>,
        interface_number: u8,
    ) -> Result<Arc<WindowsInterface>, Error> {
        let path = find_device_interface_path(self.devinst, interface_number)?;

        log::debug!(
            "Claiming device {:?} interface {interface_number} with interface path `{path}`",
            self.devinst
        );

        let handle = create_file(&path)?;

        super::events::register(&handle)?;

        let winusb_handle = unsafe {
            let mut h = 0;
            if WinUsb_Initialize(raw_handle(&handle), &mut h) == FALSE {
                error!("WinUsb_Initialize failed: {:?}", io::Error::last_os_error());
                return Err(io::Error::last_os_error());
            }
            h
        };

        Ok(Arc::new(WindowsInterface {
            handle,
            device: self.clone(),
            interface_number,
            winusb_handle,
        }))
    }

    /// For composite devices claiming an associated interface.
    ///
    /// Should be called after obtaining default `WindowsInterface` via `claim_interface`
    pub(crate) fn claim_associated_interface(
        self: &Arc<Self>,
        default_interface: &WindowsInterface,
        interface_number: u8,
    ) -> Result<Arc<WindowsInterface>, Error> {
        if interface_number == 0 {
            return Err(io::Error::new(
                ErrorKind::Other,
                "Expect a non-zero interface number",
            ));
        } else {
            let mut h = 0;
            unsafe {
                if WinUsb_GetAssociatedInterface(
                    default_interface.winusb_handle,
                    interface_number - 1,
                    &mut h,
                ) == FALSE
                {
                    error!(
                        "WinUsb_GetAssociatedInterface failed: {:?}",
                        io::Error::last_os_error()
                    );

                    return Err(io::Error::last_os_error());
                }
            };
            Ok(Arc::new(WindowsInterface {
                handle: default_interface.handle.try_clone()?,
                device: self.clone(),
                interface_number: interface_number,
                winusb_handle: h,
            }))
        }
    }

    pub(crate) fn detach_and_claim_interface(
        self: &Arc<Self>,
        interface: u8,
    ) -> Result<Arc<WindowsInterface>, Error> {
        self.claim_interface(interface)
    }
}

pub(crate) struct WindowsInterface {
    pub(crate) handle: OwnedHandle,
    pub(crate) device: Arc<WindowsDevice>,
    pub(crate) interface_number: u8,
    pub(crate) winusb_handle: WINUSB_INTERFACE_HANDLE,
}

impl WindowsInterface {
    pub(crate) fn make_transfer(
        self: &Arc<Self>,
        endpoint: u8,
        ep_type: EndpointType,
    ) -> TransferHandle<super::TransferData> {
        TransferHandle::new(super::TransferData::new(self.clone(), endpoint, ep_type))
    }

    /// Waits for a time-out interval, in milliseconds, before canceling the request
    pub(crate) fn set_timeout_millisecond(
        self: &Arc<Self>,
        endpoint: u8,
        timeout_millisecond: u32,
    ) {
        let length: u32 = size_of_val(&endpoint) as u32;
        unsafe {
            let r = WinUsb_SetPipePolicy(
                self.winusb_handle,
                endpoint,
                PIPE_TRANSFER_TIMEOUT,
                length,
                &timeout_millisecond as *const u32 as *const c_void,
            );
            if r == 1 {
                debug!(
                    "WinUsb_SetPipePolicy succeeded to set timeout to {timeout_millisecond} ms on endpoint 0x{endpoint:02X}"
                );
            } else {
                warn!("WinUsb_SetPipePolicy failed to set timeout to {timeout_millisecond} ms on endpoint 0x{endpoint:02X}");
            }
        }
    }

    /// Bypasses queuing and error handling to boost performance for multiple read requests
    pub(crate) fn enable_raw_io(self: &Arc<Self>, endpoint: u8, enable: bool) {
        let enable: u32 = if enable { 1 } else { 0 };
        let length: u32 = size_of_val(&enable) as u32;
        unsafe {
            let r = WinUsb_SetPipePolicy(
                self.winusb_handle,
                endpoint,
                RAW_IO,
                length,
                &enable as *const u32 as *const c_void,
            );
            if r == 1 {
                debug!(
                    "WinUsb_SetPipePolicy succeeded to enable/disable RAW_IO on endpoint 0x{endpoint:02X}"
                );
            } else {
                warn!("WinUsb_SetPipePolicy failed to enable/disable RAW_IO on endpoint 0x{endpoint:02X}");
            }
        }

        use windows_sys::Win32::Devices::Usb::WinUsb_GetPipePolicy;
        use windows_sys::Win32::Devices::Usb::MAXIMUM_TRANSFER_SIZE;
        let mut value: u32 = 0;
        let mut length: u32 = size_of_val(&value) as u32;
        let r = unsafe {
            WinUsb_GetPipePolicy(
                self.winusb_handle,
                endpoint,
                MAXIMUM_TRANSFER_SIZE,
                &mut length as *mut u32,
                &mut value as *mut u32 as *mut _, //std::ptr::addr_of_mut!(value) as *mut _,
            )
        };

        if r == 1 {
            debug!("WinUsb_GetPipePolicy succeeded to read MAXIMUM_TRANSFER_SIZE = {value}");
        } else {
            warn!("WinUsb_GetPipePolicy failed to read MAXIMUM_TRANSFER_SIZE");
        }

        let enable: u32 = 1;
        let length: u32 = size_of_val(&enable) as u32;
        unsafe {
            let r = WinUsb_SetPipePolicy(
                self.winusb_handle,
                endpoint,
                IGNORE_SHORT_PACKETS,
                length,
                &enable as *const u32 as *const c_void,
            );
            if r == 1 {
                debug!(
                    "WinUsb_SetPipePolicy succeeded to ignore IGNORE_SHORT_PACKETS on endpoint 0x{endpoint:02X}"
                );
            } else {
                warn!("WinUsb_SetPipePolicy failed to ignore IGNORE_SHORT_PACKETS on endpoint 0x{endpoint:02X}");
            }
        }
    }

    /// SAFETY: `data` must be valid for `len` bytes to read or write, depending on `Direction`
    unsafe fn control_blocking(
        &self,
        direction: Direction,
        control: Control,
        data: *mut u8,
        len: usize,
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        info!("Blocking control {direction:?}, {len} bytes");

        if control.recipient == Recipient::Interface && control.index as u8 != self.interface_number
        {
            warn!("WinUSB sends interface number instead of passed `index` when performing a control transfer with `Recipient::Interface`");
        }

        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let r = WinUsb_SetPipePolicy(
            self.winusb_handle,
            0,
            PIPE_TRANSFER_TIMEOUT,
            size_of_val(&timeout_ms) as u32,
            &timeout_ms as *const u32 as *const c_void,
        );

        if r != TRUE {
            error!(
                "WinUsb_SetPipePolicy PIPE_TRANSFER_TIMEOUT failed: {}",
                io::Error::last_os_error()
            );
        }

        let pkt = WINUSB_SETUP_PACKET {
            RequestType: control.request_type(direction),
            Request: control.request,
            Value: control.value,
            Index: control.index,
            Length: len.try_into().expect("request size too large"),
        };

        let mut actual_len = 0;

        let r = WinUsb_ControlTransfer(
            self.winusb_handle,
            pkt,
            data,
            len.try_into().expect("request size too large"),
            &mut actual_len,
            null_mut(),
        );

        if r == TRUE {
            Ok(actual_len as usize)
        } else {
            error!(
                "WinUsb_ControlTransfer failed: {}",
                io::Error::last_os_error()
            );
            Err(super::transfer::map_error(GetLastError()))
        }
    }

    pub fn control_in_blocking(
        &self,
        control: Control,
        data: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        unsafe {
            self.control_blocking(
                Direction::In,
                control,
                data.as_mut_ptr(),
                data.len(),
                timeout,
            )
        }
    }

    pub fn control_out_blocking(
        &self,
        control: Control,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransferError> {
        // When passed a pointer to read-only memory (e.g. a constant slice),
        // WinUSB fails with "Invalid access to memory location. (os error 998)".
        // I assume the kernel is checking the pointer for write access
        // regardless of the transfer direction. Copy the data to the stack to ensure
        // we give it a pointer to writable memory.
        let mut buf = [0; 4096];
        let Some(buf) = buf.get_mut(..data.len()) else {
            error!(
                "Control transfer length {} exceeds limit of 4096",
                data.len()
            );
            return Err(TransferError::Unknown);
        };
        buf.copy_from_slice(data);

        unsafe {
            self.control_blocking(
                Direction::Out,
                control,
                buf.as_mut_ptr(),
                buf.len(),
                timeout,
            )
        }
    }

    pub fn set_alt_setting(&self, alt_setting: u8) -> Result<(), Error> {
        unsafe {
            let r = WinUsb_SetCurrentAlternateSetting(self.winusb_handle, alt_setting.into());
            if r == TRUE {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    pub fn clear_halt(&self, endpoint: u8) -> Result<(), Error> {
        debug!("Clear halt, endpoint {endpoint:02x}");
        unsafe {
            let r = WinUsb_ResetPipe(self.winusb_handle, endpoint);
            if r == TRUE {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

impl Drop for WindowsInterface {
    fn drop(&mut self) {
        unsafe {
            WinUsb_Free(self.winusb_handle);
        }
    }
}
