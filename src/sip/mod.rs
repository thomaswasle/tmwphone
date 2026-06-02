mod ffi;

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;

// ── Public event type ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SipEvent {
    Registered,
    RegistrationFailed(String),
    IncomingCall { from: String },
    CallConnected,
    /// Fired after CallConnected once SDP has been negotiated.
    /// The app should start the RTP audio session with these parameters.
    CallMedia { local_rtp_port: u16, remote_ip: String, remote_rtp_port: u16, codec: u8 },
    CallEnded,
    CallFailed(String),
    TransferOk,
    TransferFailed(String),
    ConsultConnected,
    ConsultMedia { local_rtp_port: u16, remote_ip: String, remote_rtp_port: u16, codec: u8 },
    ConsultEnded,
}

// ── Config passed to register() ───────────────────────────────────────────────

pub struct SipConfig {
    pub server: String,
    pub username: String,
    pub password: String,
    pub display_name: String,
    pub port: u16,
}

// ── Engine ────────────────────────────────────────────────────────────────────

// Type-erased event handler stored on the heap so we can pass a thin pointer
// to the C layer as userdata.
type HandlerBox = Box<dyn FnMut(SipEvent)>;

pub struct SipEngine {
    ctx: *mut ffi::SofiaCtx,
    // Keeps the Box<HandlerBox> alive; its raw ptr is held by the C callback.
    _handler: *mut HandlerBox,
}

impl fmt::Debug for SipEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SipEngine").finish_non_exhaustive()
    }
}

impl Drop for SipEngine {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                ffi::sofia_ctx_destroy(self.ctx);
            }
            if !self._handler.is_null() {
                drop(Box::from_raw(self._handler));
            }
        }
    }
}

// Raw pointers are not Send by default; SipEngine must stay on the GTK main
// thread, which is the only thread where GLib main context runs anyway.
// Explicitly not Send.

impl SipEngine {
    /// Create a new engine.  `on_event` will be called on the GTK main thread
    /// whenever the SIP stack fires an event (sofia runs on the GLib main loop).
    pub fn new(
        server: &str,
        port: u16,
        proxy: &str,
        transport: c_int,
        tls_verify: bool,
        tls_ca_file: &str,
        on_event: impl FnMut(SipEvent) + 'static,
    ) -> Self {
        // Double-box so we get a thin (data-only) pointer suitable for c_void.
        let handler: *mut HandlerBox = Box::into_raw(Box::new(Box::new(on_event)));

        let server_c = CString::new(server).unwrap_or_default();
        let proxy_c = CString::new(proxy).unwrap_or_default();
        let tls_ca_c = CString::new(tls_ca_file).unwrap_or_default();
        let ctx = unsafe {
            ffi::sofia_ctx_create(
                server_c.as_ptr(),
                port as c_int,
                proxy_c.as_ptr(),
                transport,
                tls_verify as c_int,
                tls_ca_c.as_ptr(),
                sofia_event_cb,
                handler as *mut c_void,
            )
        };
        if ctx.is_null() {
            log::error!("sofia_ctx_create failed — SIP stack could not initialize");
            // Fire the failure event synchronously before returning a dead engine.
            unsafe {
                let cb: &mut HandlerBox = &mut *handler;
                cb(SipEvent::RegistrationFailed("SIP stack failed to start".into()));
                drop(Box::from_raw(handler));
            }
            return SipEngine { ctx: std::ptr::null_mut(), _handler: std::ptr::null_mut() };
        }

        SipEngine { ctx, _handler: handler }
    }

    pub fn register(&self, config: SipConfig) {
        let server = CString::new(config.server).unwrap();
        let user = CString::new(config.username).unwrap();
        let password = CString::new(config.password).unwrap();
        let display_name = CString::new(config.display_name).unwrap();
        unsafe {
            ffi::sofia_register(
                self.ctx,
                server.as_ptr(),
                config.port as c_int,
                user.as_ptr(),
                password.as_ptr(),
                display_name.as_ptr(),
            );
        }
    }

    pub fn unregister(&self) {
        unsafe { ffi::sofia_unregister(self.ctx) }
    }

    pub fn reregister(&self) {
        if self.ctx.is_null() { return; }
        unsafe { ffi::sofia_reregister(self.ctx) }
    }

    pub fn make_call(&self, number: &str) {
        let s = CString::new(number).unwrap();
        unsafe { ffi::sofia_call(self.ctx, s.as_ptr()) }
    }

    pub fn answer_call(&self) {
        unsafe { ffi::sofia_answer(self.ctx) }
    }

    pub fn hangup(&self) {
        unsafe { ffi::sofia_hangup(self.ctx) }
    }

    pub fn set_muted(&self, muted: bool) {
        log::debug!("set_muted({muted}) — audio not yet implemented");
    }

    pub fn set_hold(&self, hold: bool) {
        if self.ctx.is_null() { return; }
        unsafe { ffi::sofia_set_hold(self.ctx, hold as c_int) }
    }

    pub fn send_dtmf(&self, digit: char) {
        if self.ctx.is_null() { return; }
        let c = digit as u8;
        if matches!(c, b'0'..=b'9' | b'*' | b'#') {
            unsafe { ffi::sofia_send_dtmf(self.ctx, c as std::ffi::c_char) }
        }
    }

    pub fn blind_transfer(&self, number: &str) {
        if self.ctx.is_null() { return; }
        let s = CString::new(number).unwrap();
        unsafe { ffi::sofia_blind_transfer(self.ctx, s.as_ptr()) }
    }

    pub fn start_consultation(&self, number: &str) {
        if self.ctx.is_null() { return; }
        let s = CString::new(number).unwrap();
        unsafe { ffi::sofia_start_consultation(self.ctx, s.as_ptr()) }
    }

    pub fn complete_transfer(&self) {
        if self.ctx.is_null() { return; }
        unsafe { ffi::sofia_complete_transfer(self.ctx) }
    }

    pub fn cancel_consultation(&self) {
        if self.ctx.is_null() { return; }
        unsafe { ffi::sofia_cancel_consultation(self.ctx) }
    }
}

// ── C callback (always fires on the GTK main thread) ─────────────────────────

unsafe extern "C" fn sofia_event_cb(
    event: c_int,
    status: c_int,
    phrase: *const c_char,
    aux: *const c_char,
    userdata: *mut c_void,
) {
    let cb: &mut HandlerBox = &mut *(userdata as *mut HandlerBox);

    let phrase_str = || {
        if phrase.is_null() {
            String::new()
        } else {
            CStr::from_ptr(phrase).to_string_lossy().into_owned()
        }
    };

    let parse_media_aux = |aux: *const c_char| -> Option<(u16, String, u16, u8)> {
        let s = if aux.is_null() {
            String::new()
        } else {
            CStr::from_ptr(aux).to_string_lossy().into_owned()
        };
        let parts: Vec<&str> = s.splitn(4, ',').collect();
        if parts.len() >= 3 {
            let local_rtp_port = parts[0].parse::<u16>().unwrap_or(0);
            let remote_ip = parts[1].to_string();
            let remote_rtp_port = parts[2].parse::<u16>().unwrap_or(0);
            let codec = if parts.len() >= 4 { parts[3].parse::<u8>().unwrap_or(0) } else { 0 };
            Some((local_rtp_port, remote_ip, remote_rtp_port, codec))
        } else {
            None
        }
    };

    let ev = match event {
        ffi::SOFIA_EV_REGISTER_OK => Some(SipEvent::Registered),
        ffi::SOFIA_EV_REGISTER_FAIL => Some(SipEvent::RegistrationFailed(format!(
            "{status} {}", phrase_str()
        ))),
        ffi::SOFIA_EV_INCOMING_CALL => {
            let from = if aux.is_null() {
                "Unknown".to_string()
            } else {
                CStr::from_ptr(aux).to_string_lossy().into_owned()
            };
            Some(SipEvent::IncomingCall { from })
        }
        ffi::SOFIA_EV_CALL_CONNECTED => Some(SipEvent::CallConnected),
        ffi::SOFIA_EV_CALL_MEDIA => {
            if let Some((local_rtp_port, remote_ip, remote_rtp_port, codec)) = parse_media_aux(aux) {
                Some(SipEvent::CallMedia { local_rtp_port, remote_ip, remote_rtp_port, codec })
            } else {
                None
            }
        }
        ffi::SOFIA_EV_CALL_ENDED => Some(SipEvent::CallEnded),
        ffi::SOFIA_EV_CALL_FAILED => Some(SipEvent::CallFailed(format!(
            "{status} {}", phrase_str()
        ))),
        ffi::SOFIA_EV_TRANSFER_OK => Some(SipEvent::TransferOk),
        ffi::SOFIA_EV_TRANSFER_FAILED => Some(SipEvent::TransferFailed(phrase_str())),
        ffi::SOFIA_EV_CONSULT_CONNECTED => Some(SipEvent::ConsultConnected),
        ffi::SOFIA_EV_CONSULT_MEDIA => {
            if let Some((local_rtp_port, remote_ip, remote_rtp_port, codec)) = parse_media_aux(aux) {
                Some(SipEvent::ConsultMedia { local_rtp_port, remote_ip, remote_rtp_port, codec })
            } else {
                None
            }
        }
        ffi::SOFIA_EV_CONSULT_ENDED => Some(SipEvent::ConsultEnded),
        _ => None,
    };

    if let Some(ev) = ev {
        log::debug!("SIP event: {ev:?}");
        cb(ev);
    }
}
