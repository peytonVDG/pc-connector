//! Windows: while streaming, ask the Wi-Fi driver for "media streaming mode" and pause
//! background network scans. Background scans cause periodic latency spikes of tens of
//! milliseconds on most laptops; this removes them. Everything is restored on drop.

#[cfg(windows)]
pub struct WlanLowLatency {
    handle: windows_sys::Win32::Foundation::HANDLE,
    interfaces: Vec<windows_sys::core::GUID>,
}

// SAFETY: the WLAN client handle may be used from any thread.
#[cfg(windows)]
unsafe impl Send for WlanLowLatency {}

#[cfg(windows)]
impl WlanLowLatency {
    pub fn engage() -> Option<WlanLowLatency> {
        use windows_sys::Win32::NetworkManagement::WiFi::*;
        unsafe {
            let mut version = 0u32;
            let mut handle = std::ptr::null_mut();
            if WlanOpenHandle(2, std::ptr::null(), &mut version, &mut handle) != 0 {
                return None;
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(handle, std::ptr::null(), &mut list) != 0 || list.is_null() {
                WlanCloseHandle(handle, std::ptr::null());
                return None;
            }
            let count = (*list).dwNumberOfItems as usize;
            let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            let mut interfaces = Vec::new();
            for info in infos {
                if info.isState != wlan_interface_state_connected {
                    continue;
                }
                let on: i32 = 1;
                let off: i32 = 0;
                let guid = info.InterfaceGuid;
                let a = WlanSetInterface(
                    handle,
                    &guid,
                    wlan_intf_opcode_media_streaming_mode,
                    4,
                    &on as *const i32 as *const _,
                    std::ptr::null(),
                );
                let b = WlanSetInterface(
                    handle,
                    &guid,
                    wlan_intf_opcode_background_scan_enabled,
                    4,
                    &off as *const i32 as *const _,
                    std::ptr::null(),
                );
                log::info!("Wi-Fi low-latency mode: streaming mode={}, background scan off={}", a == 0, b == 0);
                interfaces.push(guid);
            }
            WlanFreeMemory(list as *const _);
            if interfaces.is_empty() {
                WlanCloseHandle(handle, std::ptr::null());
                return None;
            }
            Some(WlanLowLatency { handle, interfaces })
        }
    }
}

#[cfg(windows)]
impl Drop for WlanLowLatency {
    fn drop(&mut self) {
        use windows_sys::Win32::NetworkManagement::WiFi::*;
        unsafe {
            for guid in &self.interfaces {
                let on: i32 = 1;
                let off: i32 = 0;
                WlanSetInterface(
                    self.handle,
                    guid,
                    wlan_intf_opcode_background_scan_enabled,
                    4,
                    &on as *const i32 as *const _,
                    std::ptr::null(),
                );
                WlanSetInterface(
                    self.handle,
                    guid,
                    wlan_intf_opcode_media_streaming_mode,
                    4,
                    &off as *const i32 as *const _,
                    std::ptr::null(),
                );
            }
            WlanCloseHandle(self.handle, std::ptr::null());
        }
    }
}

#[cfg(not(windows))]
pub struct WlanLowLatency;

#[cfg(not(windows))]
impl WlanLowLatency {
    pub fn engage() -> Option<WlanLowLatency> {
        None
    }
}
