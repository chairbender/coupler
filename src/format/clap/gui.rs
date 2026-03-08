use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::future::Future;
use std::ops::Index;
use std::os::raw::c_int;
use std::rc::Rc;
use std::sync::Arc;

use super::instance::{Instance, MainThreadState};
use crate::params::{ParamId, ParamValue};
use crate::plugin::Plugin;
use crate::sync::param_gestures::ParamGestures;
use crate::view::{ParentWindow, RawParent, View, ViewHost, ViewHostInner};
use clap_sys::ext::note_ports::{clap_note_port_info, clap_plugin_note_ports};
use clap_sys::ext::posix_fd_support::{
    clap_plugin_posix_fd_support, clap_posix_fd_flags, CLAP_POSIX_FD_READ,
};
use clap_sys::ext::timer_support::clap_plugin_timer_support;
use clap_sys::ext::{gui::*, params::*};
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::{host::*, plugin::*};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use log::info;
use crate::bus::BusDir;

pub struct ClapViewHost {
    host: *const clap_host,
    host_params: Option<*const clap_host_params>,
    param_map: Arc<HashMap<ParamId, usize>>,
    param_gestures: Arc<ParamGestures>,
    #[cfg_attr(not(target_os = "linux"), allow(unused))]
    timer_id: Option<clap_id>,
}

impl ViewHostInner for ClapViewHost {
    fn begin_gesture(&self, id: ParamId) {
        self.param_gestures.begin_gesture(self.param_map[&id]);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }

    fn end_gesture(&self, id: ParamId) {
        self.param_gestures.end_gesture(self.param_map[&id]);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }

    fn set_param(&self, id: ParamId, value: ParamValue) {
        self.param_gestures.set_value(self.param_map[&id], value);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }
}

impl<P: Plugin> Instance<P> {
    pub(super) const GUI: clap_plugin_gui = clap_plugin_gui {
        is_api_supported: Some(Self::gui_is_api_supported),
        get_preferred_api: Some(Self::gui_get_preferred_api),
        create: Some(Self::gui_create),
        destroy: Some(Self::gui_destroy),
        set_scale: Some(Self::gui_set_scale),
        get_size: Some(Self::gui_get_size),
        can_resize: Some(Self::gui_can_resize),
        get_resize_hints: Some(Self::gui_get_resize_hints),
        adjust_size: Some(Self::gui_adjust_size),
        set_size: Some(Self::gui_set_size),
        set_parent: Some(Self::gui_set_parent),
        set_transient: Some(Self::gui_set_transient),
        suggest_title: Some(Self::gui_suggest_title),
        show: Some(Self::gui_show),
        hide: Some(Self::gui_hide),
    };

    #[cfg(target_os = "windows")]
    const API: &'static CStr = CLAP_WINDOW_API_WIN32;

    #[cfg(target_os = "macos")]
    const API: &'static CStr = CLAP_WINDOW_API_COCOA;

    #[cfg(target_os = "linux")]
    const API: &'static CStr = CLAP_WINDOW_API_X11;

    #[cfg(target_os = "linux")]
    pub(crate) const TIMER_SUPPORT: clap_plugin_timer_support = clap_plugin_timer_support {
        on_timer: Some(Self::timer_support_on_timer),
    };

    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_FD_SUPPORT: clap_plugin_posix_fd_support =
        clap_plugin_posix_fd_support {
            on_fd: Some(Self::posix_fd_support_on_fd),
        };

    // todo: right place?
    pub(crate) const NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
        count: Some(Self::note_ports_count),
        get: Some(Self::note_ports_get),
    };

    unsafe extern "C" fn gui_is_api_supported(
        _plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        if is_floating {
            return false;
        }

        CStr::from_ptr(api) == Self::API
    }

    unsafe extern "C" fn gui_get_preferred_api(
        _plugin: *const clap_plugin,
        api: *mut *const c_char,
        is_floating: *mut bool,
    ) -> bool {
        *is_floating = false;

        *api = Self::API.as_ptr();

        true
    }

    unsafe extern "C" fn gui_create(
        plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        if !Self::gui_is_api_supported(plugin, api, is_floating) {
            return false;
        }

        true
    }

    unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
        let instance = &*(plugin as *const Self);
        let main_thread_state = &mut *instance.main_thread_state.get();

        if let Some(posix_fd_support) = (*instance.host_extensions.get()).posix_fd_support {
            if let Some(fd) = main_thread_state.view.as_ref().unwrap().file_descriptor() {
                (*posix_fd_support).unregister_fd.unwrap_unchecked()(instance.host, fd);
            }
        }

        if let Some(timer_support) = (*instance.host_extensions.get()).timer_support {
            (*timer_support).unregister_timer.unwrap_unchecked()(
                instance.host, main_thread_state.view_host.as_ref().unwrap().timer_id.unwrap()
            );
        }

        main_thread_state.view = None;
    }

    unsafe extern "C" fn gui_set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
        false
    }

    unsafe extern "C" fn gui_get_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        let size = P::info().size;
        *width = size.width as u32;
        *height = size.height as u32;

        true
    }

    unsafe extern "C" fn gui_can_resize(_plugin: *const clap_plugin) -> bool {
        false
    }

    unsafe extern "C" fn gui_get_resize_hints(
        _plugin: *const clap_plugin,
        _hints: *mut clap_gui_resize_hints,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_adjust_size(
        _plugin: *const clap_plugin,
        _width: *mut u32,
        _height: *mut u32,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_set_size(
        _plugin: *const clap_plugin,
        _width: u32,
        _height: u32,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_set_parent(
        plugin: *const clap_plugin,
        window: *const clap_window,
    ) -> bool {
        let window = &*window;

        if CStr::from_ptr(window.api) != Self::API {
            return false;
        }

        #[cfg(target_os = "windows")]
        let raw_parent = { RawParent::Win32(window.specific.win32) };

        #[cfg(target_os = "macos")]
        let raw_parent = { RawParent::Cocoa(window.specific.cocoa) };

        #[cfg(target_os = "linux")]
        let raw_parent = { RawParent::X11(window.specific.x11) };

        let instance = &*(plugin as *const Self);
        let main_thread_state = &mut *instance.main_thread_state.get();

        let mut timer_id: Option<clap_id> = None;
        #[cfg(target_os = "linux")]
        {
            let host_extensions = instance.host_extensions.get();
            if (*host_extensions).timer_support.is_none()
            {
                dbg!("missing timer support");
                return false;
            }
            let timer_support = (*host_extensions).timer_support.unwrap();
            const TIMER_PERIOD_MS: u32 = 16;
            let mut maybe_timer_id = CLAP_INVALID_ID;
            if !(*timer_support).register_timer.unwrap_unchecked()(
                instance.host,
                TIMER_PERIOD_MS,
                &mut maybe_timer_id,
            ) {
                dbg!("Failed to register timer");
                return false;
            }
            timer_id = Some(maybe_timer_id);
        }

        let view_host = Rc::new(ClapViewHost {
            host: instance.host,
            host_params: main_thread_state.host_params,
            param_map: Arc::clone(&instance.param_map),
            param_gestures: Arc::clone(&instance.param_gestures),
            timer_id,
        });
        main_thread_state.view_host = Some(view_host);
        let view_host = ViewHost::from_inner(main_thread_state.view_host.as_ref().unwrap().clone());
        let parent = ParentWindow::from_raw(raw_parent);
        let view = main_thread_state.plugin.view(view_host, &parent);

        #[cfg(target_os = "linux")]
        {
            let host_extensions = instance.host_extensions.get();
            if (*host_extensions).posix_fd_support.is_none()
            {
                dbg!("missing fd support");
                return false;
            }
            let posix_fd_support = (*host_extensions).posix_fd_support.unwrap();

            if let Some(fd) = view.file_descriptor() {
                if !(*posix_fd_support).register_fd.unwrap_unchecked()(
                    instance.host,
                    fd,
                    CLAP_POSIX_FD_READ,
                ) {
                    dbg!("Failed to register fd");
                    return false;
                }
            }
        }

        main_thread_state.view = Some(view);

        true
    }

    unsafe extern "C" fn gui_set_transient(
        _plugin: *const clap_plugin,
        _window: *const clap_window,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {}

    unsafe extern "C" fn gui_show(_plugin: *const clap_plugin) -> bool {
        false
    }

    unsafe extern "C" fn gui_hide(_plugin: *const clap_plugin) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    unsafe extern "C" fn timer_support_on_timer(plugin: *const clap_plugin, timer_id: clap_id) {
        let instance = &*(plugin as *const Self);
        let main_thread_state = unsafe { &mut *instance.main_thread_state.get() };

        main_thread_state.view_host.as_ref().unwrap().timer_id;

        if let Some(view_host) = &mut main_thread_state.view_host {
            if let Some(id) = view_host.timer_id {
                if id == timer_id {
                    if let Some(view) = &mut main_thread_state.view {
                        view.poll();
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub unsafe extern "C" fn posix_fd_support_on_fd(
        plugin: *const clap_plugin,
        fd: i32,
        _flags: clap_posix_fd_flags,
    ) {
        let instance = &*(plugin as *const Self);
        let main_thread_state = unsafe { &mut *instance.main_thread_state.get() };
        if let Some(view) = &mut main_thread_state.view {
            if let Some(fd) = view.file_descriptor() {
                if fd == fd {
                    view.poll();
                }
            }
        }
    }

    pub unsafe extern "C" fn note_ports_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
        let instance = &*(plugin as *const Self);
        // todo: what should we do about inout? Is that even allowed?
        instance.info.event_buses.iter()
            .filter(|info| if is_input {
                info.dir == BusDir::In
            } else {
                info.dir == BusDir::Out
            }).count() as u32
    }

    pub unsafe extern "C" fn note_ports_get(
        plugin: *const clap_plugin,
        index: u32,
        is_input: bool,
        info: *mut clap_note_port_info,
    ) -> bool {
        let instance = &*(plugin as *const Self);
        let buses = instance.info.event_buses.iter()
            .filter(|info| if is_input {
                info.dir == BusDir::In
            } else {
                info.dir == BusDir::Out
            }).collect::<Vec<_>>();
        let bus_info = buses[index as usize];
        let bytes = bus_info.name.as_bytes();
        let dest = &mut (*info).name;
        *dest = [0; CLAP_NAME_SIZE];
        // when name is too long, truncate
        let limit = bytes.len().min(CLAP_NAME_SIZE - 1);
        for i in 0..limit {
            dest[i] = bytes[i] as c_char;
        }

        true
    }
}
